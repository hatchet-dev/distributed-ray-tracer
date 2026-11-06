//! Volumes (fire and smoke), from Fidget density and temperature fields.
//!
//! Standard heterogeneous-media techniques, as in pbrt-v4's volumetric path tracer
//! (https://pbr-book.org/4ed/Light_Transport_II_Volume_Rendering) and Novák et al.'s
//! "Monte Carlo methods for volumetric light transport simulation" (EG 2018):
//!
//! - Extinction σ_t = density. A volume may emit (fire: blackbody Le at the local temperature)
//!   and/or scatter (smoke: single-scattering albedo `color`, Henyey-Greenstein phase function).
//! - **Delta tracking** samples tentative collisions against a majorant σ_maj ≥ σ_t. At each one,
//!   emission σ_t/σ_maj · Le is added; with probability σ_t/σ_maj the collision is real, and then
//!   the path scatters with probability equal to the albedo (continuing in a direction drawn
//!   from the phase function) or is absorbed; otherwise it is a null collision and continues.
//! - **Ratio tracking** estimates transmittance for shadow rays: multiply by (1 − σ/σ_maj) at
//!   each tentative collision.
//! - Majorants come from a **majorant grid** traversed cell by cell. Each cell's majorant is the
//!   upper bound Fidget's interval arithmetic proves for the density over that cell, so the
//!   bounds are tight and guaranteed.

use std::sync::Mutex;

use fidget::{
    context::Tree,
    eval::{Function, TracingEvaluator},
    shape::{ShapeTape, ShapeTracingEval},
    types::Interval,
    vm::{VmFunction, VmShape},
};
use rand::{rngs::StdRng, Rng};
use rpt::{glm, Color, Ray};

type PointEval = <VmFunction as Function>::PointEval;
type PointTape = ShapeTape<<PointEval as TracingEvaluator>::Tape>;

/// Densities above this are clamped (keeps majorants finite and marching bounded).
pub const MAX_DENSITY: f64 = 50.0;
/// Cells per axis in the majorant grid.
const GRID: usize = 24;
/// Tentative collisions per tracked segment before giving up (a safety net).
const MAX_COLLISIONS: usize = 4096;
/// Loop iterations (cell crossings plus collisions) per tracked segment before giving up, so a
/// pathological ray can never stall a tile.
const MAX_ITERATIONS: usize = 8 * GRID + MAX_COLLISIONS;

/// Temperature in kelvin: a constant, or a field over space.
pub enum Temperature {
    Constant(f64),
    Field(Tree),
}

/// What a script asks for: a density field (extinction per unit length; non-positive means
/// empty), an optional temperature (makes it glow), an optional color (makes it scatter light,
/// as smoke), the phase function's anisotropy, and a brightness multiplier for the glow.
pub struct VolumeSpec {
    pub density: Tree,
    pub temperature: Option<Temperature>,
    pub albedo: Option<Color>,
    pub anisotropy: f64,
    pub brightness: f64,
}

/// What happened to a path tracked through a volume.
#[derive(Clone, Copy)]
pub enum Event {
    /// Went through without a real collision.
    Passed,
    /// Absorbed at ray parameter `t`: nothing beyond contributes.
    Absorbed { t: f64 },
    /// Scattered at ray parameter `t`; continue with the path weight multiplied by `weight`.
    Scattered { t: f64, weight: Color },
}

impl Event {
    /// Where the real collision happened (infinity if none).
    pub fn t(&self) -> f64 {
        match self {
            Event::Passed => f64::INFINITY,
            Event::Absorbed { t } | Event::Scattered { t, .. } => *t,
        }
    }
}

pub struct Volume {
    density: PointTape,
    /// `None`: doesn't glow.
    emission: Option<(Option<PointTape>, f64)>,
    brightness: f64,
    albedo: Color,
    /// Probability a real collision scatters (the albedo's average); the color ratio goes into
    /// the path weight.
    scatter_probability: f64,
    pub anisotropy: f64,
    min: [f64; 3],
    cell: [f64; 3],
    majorants: Vec<f64>,
    evals: Mutex<ShapeTracingEval<PointEval>>,
}

/// Bounding box of where `density > 0` could hold (above the floor), via Fidget's intervals.
pub fn density_bounds(density: &Tree) -> Option<([f32; 3], [f32; 3])> {
    // `bounds_of` finds where a shape is <= 0, so negate: -density <= 0 where density >= 0.
    crate::implicit::bounds_of(&VmShape::from(-density.clone()))
}

impl Volume {
    /// `None` when the volume is empty everywhere visible.
    pub fn new(spec: VolumeSpec) -> Option<Volume> {
        let (bmin, bmax) = density_bounds(&spec.density)?;
        let min = [0, 1, 2].map(|i| f64::from(bmin[i]) - 1e-3);
        let max = [0, 1, 2].map(|i| f64::from(bmax[i]) + 1e-3);
        let cell = [0, 1, 2].map(|i| (max[i] - min[i]) / GRID as f64);

        let shape = VmShape::from(spec.density);
        let itape = shape.interval_tape(Default::default());
        let mut ieval = VmShape::new_interval_eval();
        let mut majorants = Vec::with_capacity(GRID * GRID * GRID);
        for k in 0..GRID {
            for j in 0..GRID {
                for i in 0..GRID {
                    let lo = [min[0] + i as f64 * cell[0], min[1] + j as f64 * cell[1], min[2] + k as f64 * cell[2]];
                    let iv = |a: usize| Interval::new(lo[a] as f32, (lo[a] + cell[a]) as f32);
                    let upper = match ieval.eval(&itape, iv(0), iv(1), iv(2)) {
                        Ok((v, _)) if !v.upper().is_nan() => f64::from(v.upper()),
                        _ => MAX_DENSITY, // unknown: assume the worst
                    };
                    majorants.push(upper.clamp(0.0, MAX_DENSITY));
                }
            }
        }

        let emission = spec.temperature.map(|t| match t {
            Temperature::Constant(k) => (None, k),
            Temperature::Field(t) => (Some(VmShape::from(t).point_tape(Default::default())), 0.0),
        });
        let albedo = spec.albedo.unwrap_or_else(|| glm::vec3(0.0, 0.0, 0.0)).map(|c| c.clamp(0.0, 1.0));
        Some(Volume {
            density: shape.point_tape(Default::default()),
            emission,
            brightness: spec.brightness,
            albedo,
            scatter_probability: (albedo.x + albedo.y + albedo.z) / 3.0,
            anisotropy: spec.anisotropy.clamp(-0.95, 0.95),
            min,
            cell,
            majorants,
            evals: Mutex::new(VmShape::new_point_eval()),
        })
    }

    fn eval(evals: &mut ShapeTracingEval<PointEval>, tape: &PointTape, p: &glm::DVec3) -> f64 {
        match evals.eval(tape, p.x as f32, p.y as f32, p.z as f32) {
            Ok((v, _)) if v.is_finite() => f64::from(v),
            _ => 0.0,
        }
    }

    /// Emitted radiance at a point: blackbody color, brighter as it gets hotter.
    fn emission(&self, evals: &mut ShapeTracingEval<PointEval>, p: &glm::DVec3) -> Option<Color> {
        let (field, constant) = self.emission.as_ref()?;
        let kelvin = match field {
            Some(tape) => Self::eval(evals, tape, p),
            None => *constant,
        };
        Some(blackbody(kelvin) * self.brightness)
    }

    /// Walk the ray through the majorant grid between `t0` and `t1`, calling `collision(t, σ/σ_maj)`
    /// at each tentative collision; it returns false to stop.
    fn walk(&self, ray: &Ray, t0: f64, t1: f64, rng: &mut StdRng, mut collision: impl FnMut(f64, f64, &mut StdRng) -> bool) {
        let max = [0, 1, 2].map(|a| self.min[a] + GRID as f64 * self.cell[a]);
        let Some((a, b)) = clip_box(ray, &self.min, &max) else { return };
        let (mut t, t_end) = (t0.max(a), t1.min(b));
        let len = ray.dir.norm();
        // Step past a cell boundary by a small fraction of a cell: rounding can place a point on
        // the boundary inside the cell it's leaving, and a tinier nudge then makes no progress.
        let nudge = 1e-6 * self.cell.iter().cloned().fold(f64::INFINITY, f64::min) / len;
        let mut evals = self.evals.lock().unwrap();
        let (mut collisions, mut iterations) = (0, 0);
        while t < t_end && collisions < MAX_COLLISIONS && iterations < MAX_ITERATIONS {
            iterations += 1;
            let p = ray.at(t);
            let idx = [0, 1, 2].map(|i| ((((p[i] - self.min[i]) / self.cell[i]).max(0.0)) as usize).min(GRID - 1));
            let lo = [0, 1, 2].map(|i| self.min[i] + idx[i] as f64 * self.cell[i]);
            let hi = [0, 1, 2].map(|i| lo[i] + self.cell[i]);
            let exit = exit_time(ray, &lo, &hi).max(t).min(t_end);
            let majorant = self.majorants[idx[0] + GRID * (idx[1] + GRID * idx[2])];
            if majorant <= 0.0 {
                t = exit.max(t) + nudge;
                continue;
            }
            // Free-flight distance against the cell's majorant (in world units, then in t).
            let step = -(1.0 - rng.gen::<f64>()).ln() / majorant / len;
            if t + step >= exit {
                t = exit.max(t) + nudge; // no collision in this cell (memoryless: resample in the next)
                continue;
            }
            t += step;
            collisions += 1;
            let sigma = Self::eval(&mut evals, &self.density, &ray.at(t)).clamp(0.0, majorant);
            drop(evals);
            let keep_going = collision(t, sigma / majorant, rng);
            evals = self.evals.lock().unwrap();
            if !keep_going {
                return;
            }
        }
    }

    /// Delta tracking from `t0` to `t1`: the emission collected at each tentative collision
    /// (with its ray parameter, so a caller combining volumes can keep only what lies before
    /// the first real collision of any of them), and what happened to the path.
    pub fn track(&self, ray: &Ray, t0: f64, t1: f64, rng: &mut StdRng) -> (Vec<(f64, Color)>, Event) {
        let mut glow = Vec::new();
        let mut event = Event::Passed;
        self.walk(ray, t0, t1, rng, |t, ratio, rng| {
            if ratio > 0.0 {
                let mut evals = self.evals.lock().unwrap();
                if let Some(le) = self.emission(&mut evals, &ray.at(t)) {
                    glow.push((t, le * ratio));
                }
            }
            if rng.gen::<f64>() >= ratio {
                return true; // null collision
            }
            event = if rng.gen::<f64>() < self.scatter_probability {
                Event::Scattered { t, weight: self.albedo / self.scatter_probability }
            } else {
                Event::Absorbed { t }
            };
            false
        });
        (glow, event)
    }

    /// Ratio tracking from `t0` to `t1`: the fraction of light that gets through.
    pub fn transmittance(&self, ray: &Ray, t0: f64, t1: f64, rng: &mut StdRng) -> f64 {
        let mut tr = 1.0;
        self.walk(ray, t0, t1, rng, |_, ratio, _| {
            tr *= 1.0 - ratio;
            tr > 1e-4
        });
        if tr > 1e-4 { tr } else { 0.0 }
    }
}

/// Henyey-Greenstein phase function value for the angle between the ray's travel direction and
/// the scattered direction (`cos_theta` = 1 is straight ahead).
pub fn phase_hg(g: f64, cos_theta: f64) -> f64 {
    let denom = 1.0 + g * g - 2.0 * g * cos_theta;
    (1.0 - g * g) / (4.0 * std::f64::consts::PI * denom * denom.sqrt())
}

/// Sample a scattered direction from Henyey-Greenstein around `forward` (a unit vector). The
/// sample is distributed exactly by the phase function, so its weight is 1.
pub fn sample_hg(g: f64, forward: &glm::DVec3, rng: &mut StdRng) -> glm::DVec3 {
    let (u1, u2): (f64, f64) = (rng.gen(), rng.gen());
    let cos_theta = if g.abs() < 1e-3 {
        1.0 - 2.0 * u1
    } else {
        let s = (1.0 - g * g) / (1.0 - g + 2.0 * g * u1);
        (1.0 + g * g - s * s) / (2.0 * g)
    }
    .clamp(-1.0, 1.0);
    let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();
    let phi = std::f64::consts::TAU * u2;
    // Orthonormal basis around `forward`.
    let helper = if forward.x.abs() > 0.9 { glm::vec3(0.0, 1.0, 0.0) } else { glm::vec3(1.0, 0.0, 0.0) };
    let u = forward.cross(&helper).normalize();
    let v = forward.cross(&u);
    (u * (sin_theta * phi.cos()) + v * (sin_theta * phi.sin()) + forward * cos_theta).normalize()
}

/// Linear-RGB color of a blackbody at `kelvin`, scaled by (T / 1500 K)^4 (Stefan-Boltzmann),
/// so hot cores are bright and cooler edges glow dim red. Color fit by Tanner Helland
/// (https://tannerhelland.com/2012/09/18/convert-temperature-rgb-algorithm-code.html).
pub fn blackbody(kelvin: f64) -> Color {
    let k = kelvin.clamp(500.0, 12000.0);
    let t = k / 100.0;
    let r = if t <= 66.0 { 255.0 } else { 329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2) };
    let g = if t <= 66.0 {
        99.470_802_586_1 * t.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7
    };
    let srgb = glm::vec3(r, g, b).map(|c| (c / 255.0).clamp(0.0, 1.0).powf(2.2));
    srgb * (k / 1500.0).powi(4)
}

fn clip_box(ray: &Ray, lo: &[f64; 3], hi: &[f64; 3]) -> Option<(f64, f64)> {
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

fn exit_time(ray: &Ray, lo: &[f64; 3], hi: &[f64; 3]) -> f64 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn ball(density: f32) -> Volume {
        // Uniform density inside a unit ball at (0, 0, 2): density * (1 if inside).
        let (x, y, z) = (Tree::x(), Tree::y(), Tree::z());
        let r = (x.square() + y.square() + (z - 2.0).square()).sqrt();
        // Positive inside, negative outside, clamped to `density` at the center region.
        let d = ((1.0 - r) * 1000.0).min(Tree::constant(density));
        Volume::new(VolumeSpec {
            density: d,
            temperature: Some(Temperature::Constant(1500.0)),
            albedo: None,
            anisotropy: 0.0,
            brightness: 1.0,
        })
        .unwrap()
    }

    /// HG sampling matches the HG density: the mean cosine of sampled directions is g.
    #[test]
    fn hg_sampling_has_mean_cosine_g() {
        let mut rng = StdRng::seed_from_u64(3);
        let fwd = glm::vec3(0.0, 0.0, 1.0);
        for g in [-0.5, 0.0, 0.3, 0.8] {
            let n = 20000;
            let mean = (0..n).map(|_| sample_hg(g, &fwd, &mut rng).z).sum::<f64>() / n as f64;
            assert!((mean - g).abs() < 0.02, "g {g}: mean cos {mean:.3}");
        }
    }

    /// Transmittance through a uniform slab matches Beer-Lambert: exp(-σ · length).
    #[test]
    fn ratio_tracking_matches_beer_lambert() {
        let v = ball(1.5);
        let ray = Ray { origin: glm::vec3(0.0, -5.0, 2.0), dir: glm::vec3(0.0, 1.0, 0.0) };
        let mut rng = StdRng::seed_from_u64(1);
        let n = 4000;
        let mean = (0..n).map(|_| v.transmittance(&ray, 0.0, 10.0, &mut rng)).sum::<f64>() / n as f64;
        let expected = (-1.5_f64 * 2.0).exp(); // through the full diameter
        assert!((mean - expected).abs() < 0.02, "mean {mean:.4} expected {expected:.4}");
    }

    /// The probability of being absorbed by delta tracking also matches Beer-Lambert.
    #[test]
    fn delta_tracking_absorbs_at_the_right_rate() {
        let v = ball(0.8);
        let ray = Ray { origin: glm::vec3(0.0, -5.0, 2.0), dir: glm::vec3(0.0, 1.0, 0.0) };
        let mut rng = StdRng::seed_from_u64(2);
        let n = 4000;
        let survived = (0..n).filter(|_| matches!(v.track(&ray, 0.0, 10.0, &mut rng).1, Event::Passed)).count() as f64 / n as f64;
        let expected = (-0.8_f64 * 2.0).exp();
        assert!((survived - expected).abs() < 0.03, "survived {survived:.4} expected {expected:.4}");
    }

    #[test]
    fn blackbody_runs_red_to_white() {
        let warm = blackbody(1500.0);
        assert!(warm.x > warm.y && warm.y > warm.z, "1500 K is orange-red: {warm:?}");
        let hot = blackbody(6500.0);
        assert!((hot.x - hot.z).abs() / hot.x < 0.1, "6500 K is near white: {hot:?}");
        assert!(blackbody(2500.0).x > blackbody(1200.0).x * 5.0, "hotter is much brighter");
    }
}
