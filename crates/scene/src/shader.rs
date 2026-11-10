//! Shaders: script functions that set a part's material, or the sky's color, point by point.
//!
//! ```rhai
//! fn marble(p, n) {
//!   let vein = 0.5 + 0.5 * sin(6.0 * p.x + 3.0 * sin(2.0 * p.z));
//!   #{ color: mix("#2d3436", "#f5f6fa", smoothstep(0.3, 0.7, vein)), roughness: 0.15 + 0.3 * vein }
//! }
//! fn sunset(d) { mix("#ff9a5a", "#1e3a6e", smoothstep(-0.05, 0.4, d.z)) }
//!
//! #{ parts: [ #{ shape: sphere([0, 0, 1], 1), shader: marble } ], sky: sunset }
//! ```
//!
//! A part's shader takes `(p, n)`: the hit point (in the part's own coordinates, before its `rotate` and `at`) and the unit surface normal, each a map with
//! `x`, `y`, `z`. It returns a map with any of `color`, `roughness`, `metallic` and `emit`, which
//! override the part's `material` there. A sky shader takes `d`, the unit direction a ray escapes
//! in, and returns a color.
//!
//! Shaders run once, when the script is parsed, with symbolic arguments, the same way shapes are
//! built: the result is one Fidget expression per output field, evaluated at each hit. So a
//! shader can't branch on its inputs (`if p.z > 1` is an error); it uses the branch-free helpers
//! registered here instead: `step`, `smoothstep`, `select`, `clamp`, `fract` and a three-argument
//! `mix` (linear interpolation, also of colors).

use std::sync::Mutex;

use fidget::{
    context::{Context, Tree},
    eval::{BulkEvaluator, Function, MathFunction, Tape, TracingEvaluator},
    types::Grad,
    var::Var,
};
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, Map, AST};
use rpt::{glm, Color, Material};

type PointEval = <crate::Func as Function>::PointEval;
type PointTape = <PointEval as TracingEvaluator>::Tape;
type GradEval = <crate::Func as Function>::GradSliceEval;
type GradTape = <GradEval as BulkEvaluator>::Tape;

/// Most operations a compiled shader may have: every hit evaluates all of them.
pub const MAX_SHADER_OPS: usize = 20_000;
/// Brightest a sky shader's color may be (after sRGB decoding, so a little over 4^2.2).
const MAX_SKY: f64 = 20.0;
/// Steepest a bump may tilt the normal: the bump's slope along the surface is capped at this
/// (tan 70°), so bumped normals stay on the outside of the surface.
const MAX_BUMP_SLOPE: f64 = 2.75;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Red,
    Green,
    Blue,
    Roughness,
    Metallic,
    Emit,
}

/// A traced shader: one expression per output field, over the position (or direction) and
/// the normal variables.
pub struct ShaderSpec {
    fields: Vec<(Field, Tree)>,
    /// Height (in world units, along the normal) that bump-maps the surface.
    bump: Option<Tree>,
    inputs: [Var; 6],
}

/// Calls a script's shader functions with symbolic arguments.
pub struct Tracing<'a> {
    engine: &'a Engine,
    ast: &'a AST,
    /// The variables standing for the normal's components (x, y, z stand for the position).
    normal: [Var; 3],
}

fn xyz(x: Tree, y: Tree, z: Tree) -> Map {
    let mut m = Map::new();
    m.insert("x".into(), Dynamic::from(x));
    m.insert("y".into(), Dynamic::from(y));
    m.insert("z".into(), Dynamic::from(z));
    m
}

impl<'a> Tracing<'a> {
    pub fn new(engine: &'a Engine, ast: &'a AST) -> Self {
        Self { engine, ast, normal: [Var::new(), Var::new(), Var::new()] }
    }

    fn inputs(&self) -> [Var; 6] {
        [Var::X, Var::Y, Var::Z, self.normal[0], self.normal[1], self.normal[2]]
    }

    fn call(&self, f: &Dynamic, what: &str, args: impl rhai::FuncArgs, signature: &str) -> Result<Dynamic, String> {
        let f = if f.is::<FnPtr>() {
            f.clone().cast::<FnPtr>()
        } else if let Ok(name) = f.clone().into_string() {
            FnPtr::new(name).map_err(|e| format!("{what}: {e}"))?
        } else {
            return Err(format!("{what} must be a function, e.g. `shader: wood` after `fn wood{signature} {{ ... }}`, got {}", f.type_name()));
        };
        f.call::<Dynamic>(self.engine, self.ast, args).map_err(|e| {
            let e = e.to_string();
            if e.contains("Function not found") {
                format!("{what} must be a function of {signature}: {e}")
            } else {
                format!("{what}: {e}")
            }
        })
    }

    /// Traces a part's shader `f(p, n)`, with `p` in the part's own coordinates (`local`: as
    /// expressions of the world's x, y, z), so a pattern turns and moves with its part. `n` is
    /// the world normal, so "up" stays up.
    pub fn material(&self, f: &Dynamic, what: &str, local: [Tree; 3]) -> Result<ShaderSpec, String> {
        let [lx, ly, lz] = local;
        let p = xyz(lx, ly, lz);
        let [nx, ny, nz] = self.normal.map(Tree::from);
        let out = self.call(f, what, (p, xyz(nx, ny, nz)), "(p, n)")?;
        if !out.is_map() {
            return Err(format!(
                "{what} must return a map such as #{{ color: [r, g, b], roughness: 0.4 }}, got {}",
                out.type_name()
            ));
        }
        let (mut fields, mut bump) = (vec![], None);
        for (key, v) in out.cast::<Map>() {
            match key.as_str() {
                "color" => {
                    let [r, g, b] = rgb(&v).map_err(|e| format!("{what}: {e}"))?;
                    fields.extend([(Field::Red, r), (Field::Green, g), (Field::Blue, b)]);
                }
                "roughness" => fields.push((Field::Roughness, scalar(&v, "roughness").map_err(|e| format!("{what}: {e}"))?)),
                "metallic" => fields.push((Field::Metallic, scalar(&v, "metallic").map_err(|e| format!("{what}: {e}"))?)),
                "emit" => fields.push((Field::Emit, scalar(&v, "emit").map_err(|e| format!("{what}: {e}"))?)),
                "bump" => bump = Some(scalar(&v, "bump").map_err(|e| format!("{what}: {e}"))?),
                other => {
                    return Err(format!(
                        "{what} returned unknown field `{other}` (expected color, roughness, metallic, emit, bump)"
                    ))
                }
            }
        }
        let spec = ShaderSpec { fields, bump, inputs: self.inputs() };
        Shader::new(&spec).map_err(|e| format!("{what}: {e}"))?; // check it compiles and isn't too big
        Ok(spec)
    }

    /// Traces a sky shader `f(d)`.
    pub fn sky(&self, f: &Dynamic) -> Result<ShaderSpec, String> {
        let out = self.call(f, "the sky shader", (xyz(Tree::x(), Tree::y(), Tree::z()),), "(d)")?;
        let [r, g, b] = rgb(&out).map_err(|e| format!("the sky shader must return a color: {e}"))?;
        let spec = ShaderSpec { fields: vec![(Field::Red, r), (Field::Green, g), (Field::Blue, b)], bump: None, inputs: self.inputs() };
        Shader::new(&spec).map_err(|e| format!("the sky shader: {e}"))?;
        Ok(spec)
    }
}

fn scalar(v: &Dynamic, field: &str) -> Result<Tree, String> {
    match val(v) {
        Some(Val::N(x)) => Ok(Tree::constant(x as f32)),
        Some(Val::T(t)) => Ok(t),
        None => Err(format!("`{field}` must be a number or an expression of p and n, got {}", v.type_name())),
    }
}

/// A color as sRGB components (decoded when shading): "#rrggbb", or [r, g, b] of numbers or expressions.
fn rgb(v: &Dynamic) -> Result<[Tree; 3], String> {
    let parts = components(v).ok_or_else(|| {
        format!("`color` must be \"#rrggbb\" or [r, g, b] (numbers or expressions), got {}", v.type_name())
    })?;
    let mut out = parts.iter().map(|c| scalar(c, "color"));
    Ok([out.next().unwrap()?, out.next().unwrap()?, out.next().unwrap()?])
}

/// The three components of a color value ("#rrggbb" or a 3-element array), if it is one.
fn components(v: &Dynamic) -> Option<[Dynamic; 3]> {
    if let Ok(s) = v.clone().into_string() {
        return crate::parts::hex_rgb(&s).map(|c| c.map(Dynamic::from));
    }
    if v.is_array() {
        let a = v.clone().cast::<rhai::Array>();
        if a.len() == 3 {
            return Some([a[0].clone(), a[1].clone(), a[2].clone()]);
        }
    }
    None
}

/// A compiled shader, evaluated at each hit (or escaped ray, for the sky).
pub struct Shader {
    tape: PointTape,
    fields: Vec<Field>,
    /// Where each input (x, y, z, nx, ny, nz) goes in the tape's variable list, if it's used.
    slots: [Option<usize>; 6],
    vars: usize,
    eval: Mutex<PointEval>,
    bump: Option<Bump>,
}

/// A compiled bump height, evaluated with its gradient.
struct Bump {
    tape: GradTape,
    slots: [Option<usize>; 6],
    vars: usize,
    eval: Mutex<GradEval>,
}

/// Compiles `trees` as one function (one output each), checking its size.
fn compile(trees: &[&Tree]) -> Result<crate::Func, String> {
    let mut ctx = Context::new();
    let nodes: Vec<_> = trees.iter().map(|t| ctx.import(t)).collect();
    let f = crate::Func::new(&ctx, &nodes).map_err(|e| format!("could not compile the shader: {e:?}"))?;
    if f.size() > MAX_SHADER_OPS {
        return Err(format!(
            "the shader is too complex ({} operations; at most {MAX_SHADER_OPS}): use fewer noise layers or loop iterations",
            f.size()
        ));
    }
    Ok(f)
}

impl Shader {
    pub fn new(spec: &ShaderSpec) -> Result<Shader, String> {
        let zero = Tree::constant(0.0); // when nothing but a bump is shaded; `eval` skips it
        let mut trees: Vec<_> = spec.fields.iter().map(|(_, t)| t).collect();
        if trees.is_empty() {
            trees.push(&zero);
        }
        // Where each input goes in a tape's variable list, and how many variables it has.
        let slots = |vars: &fidget::var::VarMap| (spec.inputs.map(|v| vars.get(&v)), vars.len());
        let tape = compile(&trees)?.point_tape(Default::default());
        let (point_slots, point_vars) = slots(&tape.vars());
        let bump = match &spec.bump {
            Some(height) => {
                let tape = compile(&[height])?.grad_slice_tape(Default::default());
                let (slots, vars) = slots(&tape.vars());
                Some(Bump { tape, slots, vars, eval: Mutex::new(crate::Func::new_grad_slice_eval()) })
            }
            None => None,
        };
        Ok(Shader {
            tape,
            fields: spec.fields.iter().map(|(f, _)| *f).collect(),
            slots: point_slots,
            vars: point_vars,
            eval: Mutex::new(crate::Func::new_point_eval()),
            bump,
        })
    }

    fn eval(&self, inputs: [f64; 6]) -> Option<Vec<f64>> {
        if self.fields.is_empty() {
            return None;
        }
        let mut vals = vec![0.0f32; self.vars];
        for (slot, v) in self.slots.iter().zip(inputs) {
            if let Some(i) = slot {
                vals[*i] = v as f32;
            }
        }
        let mut eval = self.eval.lock().unwrap();
        let (out, _) = eval.eval(&self.tape, &vals).ok()?;
        Some(out.iter().map(|v| f64::from(*v)).collect())
    }

    /// Sets `material`'s shaded fields for a hit at `p` with unit normal `n`. Fields that come
    /// out NaN or infinite keep the material's own value.
    pub fn shade(&self, material: &mut Material, p: &glm::DVec3, n: &glm::DVec3) {
        let Some(out) = self.eval([p.x, p.y, p.z, n.x, n.y, n.z]) else { return };
        for (field, v) in self.fields.iter().zip(out) {
            if !v.is_finite() {
                continue;
            }
            match field {
                Field::Red => material.color.x = decode(v.clamp(0.0, 1.0)),
                Field::Green => material.color.y = decode(v.clamp(0.0, 1.0)),
                Field::Blue => material.color.z = decode(v.clamp(0.0, 1.0)),
                Field::Roughness => material.roughness = v.clamp(0.01, 1.0),
                Field::Metallic => material.metallic = v.clamp(0.0, 1.0),
                Field::Emit => material.emittance = v.clamp(0.0, 100.0),
            }
        }
    }

    /// The normal to shade a hit at `p` with: the surface's unit normal `n`, tilted by the slope
    /// of the shader's `bump` height along the surface (as if the surface were raised by the bump
    /// along `n`), or `n` itself without a bump.
    pub fn bumped_normal(&self, p: &glm::DVec3, n: &glm::DVec3) -> glm::DVec3 {
        let Some(bump) = &self.bump else { return *n };
        // Differentiate with respect to position only; the normal inputs are held fixed.
        let seeds = [
            Grad::new(p.x as f32, 1.0, 0.0, 0.0),
            Grad::new(p.y as f32, 0.0, 1.0, 0.0),
            Grad::new(p.z as f32, 0.0, 0.0, 1.0),
            Grad::new(n.x as f32, 0.0, 0.0, 0.0),
            Grad::new(n.y as f32, 0.0, 0.0, 0.0),
            Grad::new(n.z as f32, 0.0, 0.0, 0.0),
        ];
        let mut vals = vec![Grad::new(0.0, 0.0, 0.0, 0.0); bump.vars];
        for (slot, g) in bump.slots.iter().zip(seeds) {
            if let Some(i) = slot {
                vals[*i] = g;
            }
        }
        let slices: Vec<&[Grad]> = vals.iter().map(std::slice::from_ref).collect();
        let mut eval = bump.eval.lock().unwrap();
        let Ok(out) = eval.eval(&bump.tape, &slices) else { return *n };
        let g = out[0][0];
        let grad = glm::vec3(f64::from(g.dx), f64::from(g.dy), f64::from(g.dz));
        if !grad.iter().all(|c| c.is_finite()) {
            return *n;
        }
        // Only the slope along the surface tilts the normal.
        let mut slope = grad - n * grad.dot(n);
        let steep = slope.norm();
        if steep > MAX_BUMP_SLOPE {
            slope *= MAX_BUMP_SLOPE / steep;
        }
        (n - slope).normalize()
    }

    /// The sky's color in direction `dir` (any length), or `None` if the shader failed there.
    pub fn sky(&self, dir: &glm::DVec3) -> Option<Color> {
        let d = dir.normalize();
        let out = self.eval([d.x, d.y, d.z, 0.0, 0.0, 0.0])?;
        let c = |v: f64| v.is_finite().then(|| decode(v.max(0.0)).min(MAX_SKY));
        Some(glm::vec3(c(out[0])?, c(out[1])?, c(out[2])?))
    }
}

/// sRGB component to linear, as the rest of the scene's colors are decoded.
fn decode(v: f64) -> f64 {
    v.powf(2.2)
}

// ---- Branch-free helpers, for shaders and shapes alike --------------------------------------

/// A plain number or a symbolic expression.
enum Val {
    N(f64),
    T(Tree),
}

fn val(d: &Dynamic) -> Option<Val> {
    if d.is::<Tree>() {
        return Some(Val::T(d.clone().cast::<Tree>()));
    }
    d.as_float().map(Val::N).ok().or_else(|| d.as_int().ok().map(|i| Val::N(i as f64)))
}

impl Val {
    fn tree(self) -> Tree {
        match self {
            Val::N(x) => Tree::constant(x as f32),
            Val::T(t) => t,
        }
    }
}

type Res = Result<Dynamic, Box<EvalAltResult>>;

fn args<const N: usize>(name: &str, ds: [&Dynamic; N]) -> Result<[Val; N], Box<EvalAltResult>> {
    let mut out = Vec::with_capacity(N);
    for d in ds {
        out.push(val(d).ok_or_else(|| -> Box<EvalAltResult> {
            format!("{name}: expected numbers or expressions, got {}", d.type_name()).into()
        })?);
    }
    Ok(out.try_into().unwrap_or_else(|_| unreachable!()))
}

/// Applies `num` if every argument is a plain number, else `tree` (to their expressions).
fn lift<const N: usize>(
    name: &str,
    ds: [&Dynamic; N],
    num: impl Fn([f64; N]) -> f64,
    tree: impl Fn([Tree; N]) -> Tree,
) -> Res {
    let vs = args(name, ds)?;
    if vs.iter().all(|v| matches!(v, Val::N(_))) {
        return Ok(Dynamic::from(num(vs.map(|v| match v {
            Val::N(x) => x,
            Val::T(_) => unreachable!(),
        }))));
    }
    Ok(Dynamic::from(tree(vs.map(Val::tree))))
}

fn k(x: f64) -> Tree {
    Tree::constant(x as f32)
}

fn clamp_tree(v: Tree, lo: Tree, hi: Tree) -> Tree {
    v.max(lo).min(hi)
}

fn mix_scalar(a: &Dynamic, b: &Dynamic, t: &Dynamic) -> Res {
    lift("mix", [a, b, t], |[a, b, t]| a + (b - a) * t, |[a, b, t]| a.clone() + (b - a) * t)
}

/// `mix(a, b, t)`: linear interpolation from `a` (t = 0) to `b` (t = 1), of numbers or colors.
fn mix(a: Dynamic, b: Dynamic, t: Dynamic) -> Res {
    match (components(&a), components(&b)) {
        (Some(ca), Some(cb)) => {
            let mut out = rhai::Array::new();
            for i in 0..3 {
                out.push(mix_scalar(&ca[i], &cb[i], &t)?);
            }
            Ok(Dynamic::from_array(out))
        }
        (None, None) => mix_scalar(&a, &b, &t),
        _ => Err("mix: interpolate two numbers or two colors (\"#rrggbb\" or [r, g, b]), not one of each".into()),
    }
}

/// `color op number` (or `number op color`) component by component, e.g. `c * 0.8 + 0.1 * glow`.
fn color_op(name: &str, a: Dynamic, b: Dynamic, num: fn(f64, f64) -> f64, tree: fn(Tree, Tree) -> Tree) -> Res {
    let (ca, cb) = (components(&a), components(&b));
    let pick = |c: &Option<[Dynamic; 3]>, d: &Dynamic, i: usize| c.as_ref().map_or_else(|| d.clone(), |c| c[i].clone());
    let mut out = rhai::Array::new();
    for i in 0..3 {
        let (x, y) = (pick(&ca, &a, i), pick(&cb, &b, i));
        out.push(lift(name, [&x, &y], |[x, y]| num(x, y), |[x, y]| tree(x, y))?);
    }
    Ok(Dynamic::from_array(out))
}

/// Registers `+ - * /` between a color array and a number or expression. (Not array with array:
/// `+` on two arrays already concatenates lists.)
fn register_color_ops(engine: &mut Engine) {
    macro_rules! op {
        ($sym:literal, $num:expr, $tree:expr) => {{
            let f = |a: Dynamic, b: Dynamic| color_op($sym, a, b, $num, $tree);
            engine.register_fn($sym, move |a: rhai::Array, b: f64| f(a.into(), b.into()));
            engine.register_fn($sym, move |a: rhai::Array, b: i64| f(a.into(), b.into()));
            engine.register_fn($sym, move |a: rhai::Array, b: Tree| f(a.into(), Dynamic::from(b)));
            engine.register_fn($sym, move |a: f64, b: rhai::Array| f(a.into(), b.into()));
            engine.register_fn($sym, move |a: i64, b: rhai::Array| f(a.into(), b.into()));
            engine.register_fn($sym, move |a: Tree, b: rhai::Array| f(Dynamic::from(a), b.into()));
        }};
    }
    op!("+", |a, b| a + b, |a, b| a + b);
    op!("-", |a, b| a - b, |a, b| a - b);
    op!("*", |a, b| a * b, |a, b| a * b);
    op!("/", |a, b| a / b, |a, b| a / b);
}

/// Registers the branch-free helpers on a script engine.
pub fn register(engine: &mut Engine) {
    register_color_ops(engine);
    engine.register_fn("mix", mix);
    // select(cond, a, b): a where cond is 1, b where it is 0 (cond from step/smoothstep; in between blends).
    engine.register_fn("select", |c: Dynamic, a: Dynamic, b: Dynamic| -> Res {
        let c = lift("select", [&c], |[c]| c.clamp(0.0, 1.0), |[c]| clamp_tree(c, k(0.0), k(1.0)))?;
        mix(b, a, c)
    });
    engine.register_fn("clamp", |v: Dynamic, lo: Dynamic, hi: Dynamic| -> Res {
        lift("clamp", [&v, &lo, &hi], |[v, lo, hi]| v.max(lo).min(hi), |[v, lo, hi]| clamp_tree(v, lo, hi))
    });
    // step(edge, v): 1 where v >= edge, else 0. Fidget's compare(a, b) is the sign of a - b.
    engine.register_fn("step", |edge: Dynamic, v: Dynamic| -> Res {
        lift(
            "step",
            [&edge, &v],
            |[edge, v]| if v >= edge { 1.0 } else { 0.0 },
            |[edge, v]| (v.compare(edge) + k(1.0)).min(k(1.0)),
        )
    });
    engine.register_fn("smoothstep", |e0: Dynamic, e1: Dynamic, v: Dynamic| -> Res {
        lift(
            "smoothstep",
            [&e0, &e1, &v],
            |[e0, e1, v]| {
                let t = ((v - e0) / (e1 - e0)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            },
            |[e0, e1, v]| {
                let t = clamp_tree((v - e0.clone()) / (e1 - e0), k(0.0), k(1.0));
                t.clone() * t.clone() * (k(3.0) - t * k(2.0))
            },
        )
    });
    engine.register_fn("fract", |v: Dynamic| -> Res {
        lift("fract", [&v], |[v]| v - v.floor(), |[v]| v.clone() - v.floor())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader(script: &str) -> Result<Shader, String> {
        let mut engine = fidget::rhai::engine();
        register(&mut engine);
        let ast = engine.compile(script).map_err(|e| e.to_string())?;
        let f: Dynamic = engine.eval_ast(&ast).map_err(|e| e.to_string())?;
        Shader::new(&Tracing::new(&engine, &ast).material(&f, "shader", [Tree::x(), Tree::y(), Tree::z()])?)
    }

    #[test]
    fn shades_by_position_and_normal() {
        let s = shader(
            r##"fn paint(p, n) { #{ color: select(step(0.0, p.x), "#ff0000", "#0000ff"), roughness: 0.2 + 0.5 * n.z } }
                Fn("paint")"##,
        )
        .unwrap();
        let mut m = Material::diffuse(glm::vec3(0.5, 0.5, 0.5));
        s.shade(&mut m, &glm::vec3(1.0, 0.0, 0.0), &glm::vec3(0.0, 0.0, 1.0));
        assert_eq!((m.color.x, m.color.z), (1.0, 0.0));
        assert!((m.roughness - 0.7).abs() < 1e-6);
        s.shade(&mut m, &glm::vec3(-1.0, 0.0, 0.0), &glm::vec3(1.0, 0.0, 0.0));
        assert_eq!((m.color.x, m.color.z), (0.0, 1.0));
        assert!((m.roughness - 0.2).abs() < 1e-6);
    }

    #[test]
    fn bump_tilts_the_normal_by_its_slope() {
        let up = glm::vec3(0.0, 0.0, 1.0);
        let at = |script: &str, n: glm::DVec3| shader(script).unwrap().bumped_normal(&glm::vec3(0.3, 0.0, 1.0), &n);
        // Height rising along +x tilts the normal toward -x: n - grad h.
        let n = at(r#"fn s(p, n) { #{ bump: 0.1 * p.x } } Fn("s")"#, up);
        let expected = glm::vec3(-0.1, 0.0, 1.0).normalize();
        assert!((n - expected).norm() < 1e-6, "{n:?}");
        // Slope across the normal (along it) doesn't tilt anything.
        assert!((at(r#"fn s(p, n) { #{ bump: 0.5 * p.z } } Fn("s")"#, up) - up).norm() < 1e-6);
        // Steep bumps are capped, so the normal stays on the outside.
        let steep = at(r#"fn s(p, n) { #{ bump: 100.0 * p.x } } Fn("s")"#, up);
        assert!((steep - glm::vec3(-2.75, 0.0, 1.0).normalize()).norm() < 1e-6, "{steep:?}");
        // A bump that depends on the normal (e.g. only on top faces) still works.
        let n = at(r#"fn s(p, n) { #{ bump: 0.1 * p.x * n.z } } Fn("s")"#, up);
        assert!((n - expected).norm() < 1e-6, "{n:?}");
        // Without a bump the normal is unchanged.
        assert_eq!(at(r#"fn s(p, n) { #{ roughness: 0.5 } } Fn("s")"#, up), up);
        assert!(shader(r#"fn s(p, n) { #{ bump: "x" } } Fn("s")"#).err().unwrap().contains("`bump` must be a number"));
    }

    #[test]
    fn helpers_work_on_plain_numbers() {
        let mut engine = fidget::rhai::engine();
        register(&mut engine);
        assert_eq!(engine.eval::<f64>("smoothstep(0.0, 1.0, 0.5)").unwrap(), 0.5);
        assert_eq!(engine.eval::<f64>("step(1, 0.5) + clamp(5, 0, 2) + fract(2.25)").unwrap(), 2.25);
        assert_eq!(engine.eval::<f64>("mix(2.0, 4.0, 0.25)").unwrap(), 2.5);
        // Colors scale and offset by numbers; arrays still concatenate.
        assert_eq!(engine.eval::<f64>("let c = mix(\"#000000\", [1.0, 0.5, 0.0], 0.5) * 2 + 0.25; c[1]").unwrap(), 0.75);
        assert_eq!(engine.eval::<i64>("([1, 2] + [3]).len()").unwrap(), 3);
    }

    #[test]
    fn colors_combine_with_expressions() {
        // The sky a model first wrote: a mixed color plus a glow expression.
        let mut engine = fidget::rhai::engine();
        register(&mut engine);
        let ast = engine
            .compile(r##"fn sky(d) { let glow = exp(-6.0 * square(d.z)); mix("#f2a65a", "#1b2a4a", d.z) + glow * 0.3 } Fn("sky")"##)
            .unwrap();
        let f: Dynamic = engine.eval_ast(&ast).unwrap();
        let s = Shader::new(&Tracing::new(&engine, &ast).sky(&f).unwrap()).unwrap();
        let horizon = s.sky(&glm::vec3(1.0, 0.0, 0.0)).unwrap();
        assert!(horizon.x > 0.9, "orange plus a full glow: {horizon:?}");
    }

    #[test]
    fn bad_shaders_are_explained() {
        let err = |s: &str| shader(s).err().unwrap();
        assert!(err(r##"fn s(p, n) { #{ colour: "#ffffff" } } Fn("s")"##).contains("unknown field `colour`"));
        assert!(err(r#"fn s(p) { #{ roughness: 0.3 } } Fn("s")"#).contains("must be a function of (p, n)"));
        assert!(err(r#"fn s(p, n) { 0.5 } Fn("s")"#).contains("must return a map"));
        assert!(err(r#"fn s(p, n) { if p.z > 1.0 { #{} } else { #{} } } Fn("s")"#).contains("cannot compare"));
        assert!(err(r#"fn s(p, n) { #{ roughness: "rough" } } Fn("s")"#).contains("`roughness` must be a number"));
    }
}
