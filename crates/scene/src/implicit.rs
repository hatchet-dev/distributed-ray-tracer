//! A Fidget implicit surface as an rpt [`Shape`].
//!
//! Fidget turns a user's Rhai script into a math expression `f(x, y, z)` (negative inside,
//! positive outside) and evaluates it. rpt only needs ray intersections, so this file is the
//! glue: march along the ray using `|f|` as a conservative step (exact for distance fields,
//! and Fidget's primitives and blends are close to that), refine the crossing by bisection,
//! and take the normal from Fidget's gradient evaluator.
//!
//! To keep marching cheap, a grid over the shape's bounding box records what Fidget's interval
//! arithmetic proves about each cell: rays jump straight through cells that are provably empty
//! or solid, and surface cells evaluate a tape Fidget simplified for that cell.

use std::sync::Mutex;

use fidget::{
    context::Tree,
    eval::{BulkEvaluator, Function, TracingEvaluator},
    shape::{ShapeBulkEval, ShapeTape, ShapeTracingEval},
    types::{Grad, Interval},
};
use rand::rngs::StdRng;
use rpt::{glm, HitRecord, Ray, Shape};

type PointEval = <crate::Func as Function>::PointEval;
type GradEval = <crate::Func as Function>::GradSliceEval;

/// Region the marcher searches; geometry outside it is not rendered.
const BOUNDS_MIN: [f64; 3] = [-20.0, -20.0, -2.0];
const BOUNDS_MAX: [f64; 3] = [20.0, 20.0, 20.0];
/// Surface tolerance and step limits, in world units.
const HIT_EPS: f64 = 1e-4;
const MIN_STEP: f64 = 2e-4;
const MAX_STEPS: usize = 512;
/// Fraction of `|f|` to step; < 1 tolerates formulas that aren't exact distances.
const STEP_SCALE: f64 = 0.8;

/// Rhai operations a script may run before it's stopped.
const MAX_SCRIPT_STEPS: u64 = 10_000_000;

/// Run a shape script with Fidget's sandboxed Rhai engine (bounded script steps and expression
/// depth) and read its result: parts with materials, plus optional lights and sky.
pub fn parse_scene(script: &str) -> Result<crate::parts::SceneSpec, String> {
    let mut engine = fidget::rhai::engine();
    crate::shader::register(&mut engine);
    // Fidget's default cap is 50,000 steps; scripts can be megabytes, so allow far more while
    // still stopping a runaway loop.
    engine.on_progress(|steps| (steps > MAX_SCRIPT_STEPS).then(|| "script runtime exceeded".into()));
    let error = |e: String| {
        // Shapes and shaders are built symbolically, so Rhai can't compare their values.
        let hint = if e.contains("cannot compare Tree types") {
            " (shapes and shaders are built as expressions of x, y, z and the normal, so `if` and comparisons can't \
             depend on them: use step, smoothstep or select instead)"
        } else {
            ""
        };
        format!("shape script: {e}{hint}")
    };
    let ast = engine.compile(script).map_err(|e| error(e.to_string()))?;
    let value = engine.eval_ast::<rhai::Dynamic>(&ast).map_err(|e| error(e.to_string()))?;
    let tracing = crate::shader::Tracing::new(&engine, &ast);
    crate::parts::scene_from_dynamic(value, &tracing).map_err(error)
}

/// Just the parts of a script (see [`parse_scene`]).
#[cfg(test)]
pub fn parse(script: &str) -> Result<Vec<crate::parts::Part>, String> {
    parse_scene(script).map(|s| s.parts)
}

/// Octree depth for [`bounds`]: cells end up ~0.3 units across.
const BOUNDS_DEPTH: u32 = 7;

/// Axis-aligned bounding box of the visible part of the shape (above the floor, inside the
/// marching bounds), using Fidget's interval arithmetic: any box where `f` is provably positive
/// is empty, so subdivide only the boxes that might contain surface or interior.
/// `None` if the shape is empty there.
pub fn bounds(tree: Tree) -> Option<([f32; 3], [f32; 3])> {
    bounds_of(&crate::FShape::from(tree))
}

/// Cells per axis in the acceleration grid over the shape's bounding box.
const GRID: usize = 16;

type PointTape = ShapeTape<<PointEval as TracingEvaluator>::Tape>;
type GradTape = ShapeTape<<GradEval as BulkEvaluator>::Tape>;
type Evals = (ShapeTracingEval<PointEval>, ShapeBulkEval<GradEval>);

/// What Fidget's interval arithmetic proved about one grid cell.
enum Cell {
    /// `f > 0` everywhere in the cell (holds the lower bound): no surface, skip it.
    Empty(f64),
    /// `f < 0` everywhere (holds the upper bound): inside the shape, no surface, skip it.
    Solid(f64),
    /// May contain surface: index of the cell's simplified tapes.
    Surface(usize),
}

/// Grid over the shape's bounding box. Each surface cell gets a tape that Fidget simplified for
/// that cell: `min`/`max` branches that can't matter there are dropped, so a script that unions
/// many parts evaluates only the nearby ones.
struct Grid {
    min: [f64; 3],
    cell: [f64; 3],
    cells: Vec<Cell>,
    tapes: Vec<(PointTape, GradTape)>,
}

impl Grid {
    fn build(shape: &crate::FShape) -> Option<Grid> {
        let (min, cell) = grid_box(bounds_of(shape)?, GRID);
        let tape = shape.interval_tape(Default::default());
        let mut eval = crate::FShape::new_interval_eval();
        let mut workspace = Default::default();
        let mut tapes = Vec::new();
        let cells = grid_cells(min, cell, GRID)
            .map(|iv| match eval.eval(&tape, iv[0], iv[1], iv[2]) {
                Ok((v, _)) if v.lower() > 0.0 => Cell::Empty(f64::from(v.lower())),
                Ok((v, _)) if v.upper() < 0.0 => Cell::Solid(f64::from(v.upper())),
                result => {
                    // Simplify the tape with this cell's trace; the full shape when there is none.
                    let local = match result {
                        Ok((_, Some(trace))) => shape.simplify(trace, Default::default(), &mut workspace).ok(),
                        _ => None,
                    };
                    let local = local.as_ref().unwrap_or(shape);
                    tapes.push((local.point_tape(Default::default()), local.grad_slice_tape(Default::default())));
                    Cell::Surface(tapes.len() - 1)
                }
            })
            .collect();
        Some(Grid { min, cell, cells, tapes })
    }

    /// Cell index and its box for a point, or `None` outside the grid. Points within a hair of
    /// the grid's faces (rays entering it land exactly there, up to rounding) snap to the edge
    /// cells.
    fn locate(&self, p: &glm::DVec3) -> Option<(usize, [f64; 3])> {
        const SNAP: f64 = 1e-6; // in cells
        let mut idx = [0usize; 3];
        for a in 0..3 {
            let u = (p[a] - self.min[a]) / self.cell[a];
            if !(-SNAP..GRID as f64 + SNAP).contains(&u) {
                return None;
            }
            idx[a] = (u.max(0.0) as usize).min(GRID - 1);
        }
        let lo = [0, 1, 2].map(|a| self.min[a] + idx[a] as f64 * self.cell[a]);
        Some((idx[0] + GRID * (idx[1] + GRID * idx[2]), lo))
    }

    fn max(&self) -> [f64; 3] {
        [0, 1, 2].map(|a| self.min[a] + GRID as f64 * self.cell[a])
    }
}

/// A grid of `n`³ cells over a bounding box, padded by a hair so surfaces on the boundary stay
/// inside: its min corner and cell size.
pub(crate) fn grid_box((bmin, bmax): ([f32; 3], [f32; 3]), n: usize) -> ([f64; 3], [f64; 3]) {
    let min = [0, 1, 2].map(|i| f64::from(bmin[i]) - 1e-3);
    let max = [0, 1, 2].map(|i| f64::from(bmax[i]) + 1e-3);
    (min, [0, 1, 2].map(|i| (max[i] - min[i]) / n as f64))
}

/// Each cell of a grid (x fastest, then y, then z) as the intervals it spans on each axis.
pub(crate) fn grid_cells(min: [f64; 3], cell: [f64; 3], n: usize) -> impl Iterator<Item = [Interval; 3]> {
    (0..n * n * n).map(move |c| {
        let idx = [c % n, c / n % n, c / (n * n)];
        [0, 1, 2].map(|a| {
            let lo = min[a] + idx[a] as f64 * cell[a];
            Interval::new(lo as f32, (lo + cell[a]) as f32)
        })
    })
}

/// Ray parameter where the ray leaves the box `[lo, hi]` (assumes it's inside).
pub(crate) fn exit_time(ray: &Ray, lo: &[f64; 3], hi: &[f64; 3]) -> f64 {
    (0..3)
        .map(|a| {
            let d = ray.dir[a];
            if d > 0.0 {
                (hi[a] - ray.origin[a]) / d
            } else if d < 0.0 {
                (lo[a] - ray.origin[a]) / d
            } else {
                f64::INFINITY
            }
        })
        .fold(f64::INFINITY, f64::min)
}

/// Clip a ray to an axis-aligned box; returns the `[t0, t1]` overlap.
pub(crate) fn clip_box(ray: &Ray, lo: &[f64; 3], hi: &[f64; 3]) -> Option<(f64, f64)> {
    let (mut t0, mut t1) = (0.0_f64, f64::INFINITY);
    for i in 0..3 {
        let inv = 1.0 / ray.dir[i];
        let (mut a, mut b) = ((lo[i] - ray.origin[i]) * inv, (hi[i] - ray.origin[i]) * inv);
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        t0 = t0.max(a);
        t1 = t1.min(b);
    }
    (t0 <= t1).then_some((t0, t1))
}

pub struct ImplicitShape {
    /// `None` when nothing is visible: every ray misses.
    grid: Option<Grid>,
    full_grad: GradTape,
    // rpt's `Shape::intersect` takes `&self`, but Fidget evaluators keep scratch state.
    // Each tile builds its own scene, so the lock is never contended.
    evals: Mutex<Evals>,
}

impl ImplicitShape {
    pub fn new(tree: Tree) -> Self {
        let shape = crate::FShape::from(tree);
        Self {
            grid: Grid::build(&shape),
            full_grad: shape.grad_slice_tape(Default::default()),
            evals: Mutex::new((crate::FShape::new_point_eval(), crate::FShape::new_grad_slice_eval())),
        }
    }

    /// `f` at a point: exact (from the cell's simplified tape) in surface cells; in empty or
    /// solid cells, the interval bound, which has the right sign; "far outside" off the grid.
    /// Also returns the box of an empty/solid cell, which a ray on that side can jump through.
    fn probe(&self, grid: &Grid, evals: &mut Evals, p: &glm::DVec3) -> (f64, Option<[f64; 3]>) {
        let Some((idx, lo)) = grid.locate(p) else { return (f64::INFINITY, None) };
        match grid.cells[idx] {
            Cell::Empty(b) | Cell::Solid(b) => (b, Some(lo)),
            Cell::Surface(t) => match evals.0.eval(&grid.tapes[t].0, p.x as f32, p.y as f32, p.z as f32) {
                Ok((v, _)) if v.is_finite() => (f64::from(v), None),
                _ => (f64::INFINITY, None), // NaN / errors: treat as empty space
            },
        }
    }

    /// The unit normal at `p`, from Fidget's gradient (straight up where it has none).
    fn normal(&self, grid: &Grid, evals: &mut Evals, p: &glm::DVec3) -> glm::DVec3 {
        let tape = match grid.locate(p).map(|(idx, _)| &grid.cells[idx]) {
            Some(Cell::Surface(t)) => &grid.tapes[*t].1,
            _ => &self.full_grad,
        };
        let (x, y, z) = (p.x as f32, p.y as f32, p.z as f32);
        let out = evals.1.eval(
            tape,
            &[Grad::new(x, 1.0, 0.0, 0.0)],
            &[Grad::new(y, 0.0, 1.0, 0.0)],
            &[Grad::new(z, 0.0, 0.0, 1.0)],
        );
        if let Ok([g]) = out.as_deref() {
            let n = glm::vec3(f64::from(g.dx), f64::from(g.dy), f64::from(g.dz));
            if n.norm() > 0.0 && n.iter().all(|c| c.is_finite()) {
                return n.normalize();
            }
        }
        glm::vec3(0.0, 0.0, 1.0)
    }
}

/// Bounding box of the visible part of a shape (see [`bounds`]).
pub(crate) fn bounds_of(shape: &crate::FShape) -> Option<([f32; 3], [f32; 3])> {
    bounds_within(shape, ([BOUNDS_MIN[0] as f32, BOUNDS_MIN[1] as f32, 0.0], BOUNDS_MAX.map(|v| v as f32)))
}

/// Bounding box of a whole shape within the marching bounds, including any part below the
/// floor (for the preview, where a part may yet be moved up).
pub fn full_bounds(tree: Tree) -> Option<([f32; 3], [f32; 3])> {
    let lo = [-BOUNDS_MAX[0], -BOUNDS_MAX[1], -BOUNDS_MAX[2]];
    bounds_within(&crate::FShape::from(tree), (lo.map(|v| v as f32), BOUNDS_MAX.map(|v| v as f32)))
}

/// Bounding box of where `shape` may be `<= 0` inside `root`, to [`BOUNDS_DEPTH`] subdivisions.
fn bounds_within(shape: &crate::FShape, root: ([f32; 3], [f32; 3])) -> Option<([f32; 3], [f32; 3])> {
    let tape = shape.interval_tape(Default::default());
    let mut eval = crate::FShape::new_interval_eval();
    let mut found: Option<([f32; 3], [f32; 3])> = None;
    let mut stack = vec![(root, 0u32)];
    while let Some(((lo, hi), depth)) = stack.pop() {
        let iv = |i: usize| Interval::new(lo[i], hi[i]);
        let maybe_filled = match eval.eval(&tape, iv(0), iv(1), iv(2)) {
            Ok((v, _)) => !(v.lower() > 0.0), // also true for NaN: be conservative
            Err(_) => true,
        };
        if !maybe_filled {
            continue;
        }
        if depth == BOUNDS_DEPTH {
            let (fmin, fmax) = found.get_or_insert((lo, hi));
            for i in 0..3 {
                fmin[i] = fmin[i].min(lo[i]);
                fmax[i] = fmax[i].max(hi[i]);
            }
            continue;
        }
        let mid = [0, 1, 2].map(|i| 0.5 * (lo[i] + hi[i]));
        for octant in 0..8 {
            let pick = |i: usize| octant >> i & 1 == 1;
            let clo = [0, 1, 2].map(|i| if pick(i) { mid[i] } else { lo[i] });
            let chi = [0, 1, 2].map(|i| if pick(i) { hi[i] } else { mid[i] });
            stack.push(((clo, chi), depth + 1));
        }
    }
    found
}

impl Shape for ImplicitShape {
    fn intersect(&self, ray: &Ray, t_min: f64, record: &mut HitRecord) -> bool {
        let Some(grid) = &self.grid else { return false };
        let Some((t0, t1)) = clip_box(ray, &grid.min, &grid.max()) else { return false };
        let len = ray.dir.norm(); // world distance per unit of t
        let t_end = t1.min(record.time);
        let nudge = 1e-6 / len; // past a cell boundary
        let mut evals = self.evals.lock().unwrap();

        let mut t = t0.max(t_min);
        let (mut f, mut skip) = self.probe(grid, &mut evals, &ray.at(t));
        // A ray leaving this surface (reflection, shadow, refraction) starts with f ~ 0:
        // step off it first so it doesn't immediately hit itself.
        let mut escapes = 0;
        while f.abs() < 10.0 * HIT_EPS && escapes < 32 {
            t += 10.0 * HIT_EPS / len;
            (f, skip) = self.probe(grid, &mut evals, &ray.at(t));
            escapes += 1;
        }
        let side = f.signum(); // +1 marching outside, -1 marching inside (e.g. glass)

        for _ in 0..MAX_STEPS {
            if t > t_end {
                return false;
            }
            // In a cell proved empty or solid there is no surface to cross: jump to its exit.
            if let Some(lo) = skip.filter(|_| f.signum() == side) {
                let hi = [0, 1, 2].map(|a| lo[a] + grid.cell[a]);
                t = exit_time(ray, &lo, &hi).max(t) + nudge;
                (f, skip) = self.probe(grid, &mut evals, &ray.at(t));
                continue;
            }
            let t_next = t + (f.abs() * STEP_SCALE).max(MIN_STEP) / len;
            let (f_next, skip_next) = self.probe(grid, &mut evals, &ray.at(t_next));
            let crossed = f_next.signum() != side;
            if crossed || f_next.abs() < HIT_EPS {
                // Bisect [t, t_next] down to the crossing.
                let (mut lo, mut hi) = (t, t_next);
                if crossed {
                    for _ in 0..24 {
                        let mid = 0.5 * (lo + hi);
                        if self.probe(grid, &mut evals, &ray.at(mid)).0.signum() == side {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                }
                let t_hit = hi;
                if t_hit > t_end {
                    return false;
                }
                record.time = t_hit;
                record.normal = self.normal(grid, &mut evals, &ray.at(t_hit));
                return true;
            }
            t = t_next;
            f = f_next;
            skip = skip_next;
        }
        false
    }

    fn sample(&self, _target: &glm::DVec3, _rng: &mut StdRng) -> (glm::DVec3, glm::DVec3, f64) {
        // Only light sources are sampled, and user shapes are never lights.
        unreachable!("implicit shapes are not used as lights")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rays from all around a sphere must hit it where the analytic intersection says.
    #[test]
    fn grid_marching_matches_analytic_sphere() {
        let tree = parse("sphere([0, 0, 1.2], 1.2)").unwrap().remove(0).tree;
        let shape = ImplicitShape::new(tree);
        let center = glm::vec3(0.0, 0.0, 1.2);
        let mut failures = vec![];
        for i in 0..64 {
            let az = i as f64 / 64.0 * std::f64::consts::TAU;
            for el in [-0.3_f64, 0.0, 0.4, 0.9] {
                let dir_out = glm::vec3(el.cos() * az.cos(), el.cos() * az.sin(), el.sin());
                let origin = center + dir_out * 10.0;
                // Aim slightly off-center so rays hit at varied angles.
                let target = center + glm::vec3(0.3 * az.sin(), 0.2, 0.1);
                let dir = (target - origin).normalize();
                let ray = Ray { origin, dir };
                // Analytic: |o + t d - c| = r.
                let oc = origin - center;
                let b = oc.dot(&dir);
                let c = oc.norm_squared() - 1.2 * 1.2;
                let expected = -b - (b * b - c).sqrt();
                let mut rec = HitRecord::new();
                let hit = shape.intersect(&ray, 1e-12, &mut rec);
                if !hit || (rec.time - expected).abs() > 1e-3 {
                    failures.push(format!("az {az:.2} el {el:.1}: hit {hit} t {:.4} expected {expected:.4} at {:?}", rec.time, ray.at(if hit { rec.time } else { expected })));
                }
            }
        }
        assert!(failures.is_empty(), "{} of 256 rays wrong:\n{}", failures.len(), failures.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
    }
}
