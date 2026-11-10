//! A shape script's result: one shape, a list of parts with materials, or a scene map that also
//! places lights:
//!
//! ```rhai
//! #{
//!   parts: [ ... ],   // or a single shape
//!   lights: [
//!     #{ sphere: [2, -3, 7], radius: 1.5, color: "#fff4e0", intensity: 10 },  // soft area light
//!     #{ point: [0, 0, 3.75], color: "#ffd27a", intensity: 4 },               // falls off with distance
//!     #{ sun: [-1, -2, 3], intensity: 1.5 },                                   // direction toward the sun
//!   ],
//!   sky: "#0b1530",   // ambient sky color
//!   camera: #{ azimuth: -60, elevation: 25, fov: 40 },  // view; unset fields are auto-framed
//!   volumes: [        // glowing gas (fire): density and temperature fields
//!     #{ density: 4 * (0.6 - sqrt(x*x + y*y + (z - 1)*(z - 1))), temperature: 1800, brightness: 1 },
//!   ],
//! }
//! ```
//!
//! ```rhai
//! [
//!   #{ shape: sphere([0, 0, 1], 1), material: #{ color: "#c0392b", roughness: 0.3 } },
//!   #{ shape: box(#{ lower: [-2, -2, 0], upper: [2, 2, 0.2] }), material: #{ metallic: 1.0 } },
//! ]
//! ```
//!
//! Material maps become rpt materials. Fields (all optional): `color` ("#rrggbb" or [r, g, b]
//! in 0..1, sRGB), `roughness` (0..1), `metallic` (0..1), `glass` (bool), `ior` (index of
//! refraction, for glass), `emit` (how bright the part glows).

use fidget::context::Tree;
use rhai::{Dynamic, Map};
use rhai::FnPtr;
use rpt::{glm, hex_color, sphere, Color, Light, Material, Object, Transformable};

use crate::shader::{ShaderSpec, Tracing};

/// Most parts one script may have; every ray tests every part.
pub const MAX_PARTS: usize = 16;
/// Most lights one script may place; every shaded point samples every light.
pub const MAX_LIGHTS: usize = 8;
/// Most volumes one script may have; rays through them are tracked step by step.
pub const MAX_VOLUMES: usize = 4;

/// Everything a script describes.
pub struct SceneSpec {
    pub parts: Vec<Part>,
    /// `None`: the default overhead light.
    pub lights: Option<Vec<Light>>,
    /// `None`: the default sky.
    pub sky: Option<Color>,
    /// A sky shader, `fn(d) -> color`, instead of one sky color.
    pub sky_shader: Option<ShaderSpec>,
    /// The floor's material (and shader); `None`: the black void.
    pub floor: Option<(Material, Option<ShaderSpec>)>,
    pub volumes: Vec<crate::volume::VolumeSpec>,
    /// The script's chosen view, if any.
    pub camera: Option<crate::CameraHint>,
}

pub struct Part {
    pub tree: Tree,
    /// `None`: use the default (clay) material.
    pub material: Option<Material>,
    /// Sets the material point by point (see `shader.rs`).
    pub shader: Option<ShaderSpec>,
}

/// Interpret a script's final value: a scene map (`parts`, `lights`, `sky`), or just parts.
pub fn scene_from_dynamic(value: Dynamic, tracing: &Tracing) -> Result<SceneSpec, String> {
    if value.is_map() {
        let mut map = value.clone().cast::<Map>();
        if ["parts", "lights", "sky", "volumes", "camera", "floor"].iter().any(|k| map.contains_key(*k)) {
            let parts = match map.remove("parts") {
                Some(p) => from_dynamic(p, tracing)?,
                None if map.contains_key("volumes") => vec![], // a scene can be just fire
                None => return Err("the scene map is missing `parts`".into()),
            };
            let volumes = match map.remove("volumes") {
                Some(v) => volumes(v)?,
                None => vec![],
            };
            let camera = match map.remove("camera") {
                Some(c) if c.is_map() => Some(camera(c.cast::<Map>())?),
                Some(c) => return Err(format!("`camera` must be a map, got {}", c.type_name())),
                None => None,
            };
            let lights = match map.remove("lights") {
                Some(l) => Some(lights(l)?),
                None => None,
            };
            let (sky, sky_shader) = match map.remove("sky") {
                Some(f) if f.is::<FnPtr>() => (None, Some(tracing.sky(&f)?)),
                Some(c) => (Some(color(&c).map_err(|e| e.replace("`color`", "`sky`"))?), None),
                None => (None, None),
            };
            let floor = match map.remove("floor") {
                Some(f) => floor(f, tracing)?,
                None => None,
            };
            if let Some(key) = map.keys().next() {
                return Err(format!("unknown scene field `{key}` (expected parts, lights, sky, volumes, camera, floor)"));
            }
            return Ok(SceneSpec { parts, lights, sky, sky_shader, floor, volumes, camera });
        }
    }
    Ok(SceneSpec {
        parts: from_dynamic(value, tracing)?,
        lights: None,
        sky: None,
        sky_shader: None,
        floor: None,
        volumes: vec![],
        camera: None,
    })
}

/// The scene's `floor`: `false` (the black void, the default), `true` (light grey), a color, or a
/// material map that may also have a `shader`.
fn floor(v: Dynamic, tracing: &Tracing) -> Result<Option<(Material, Option<ShaderSpec>)>, String> {
    if let Ok(on) = v.as_bool() {
        return Ok(on.then(|| (Material::diffuse(hex_color(0xE7E7E7)), None)));
    }
    if v.is_map() {
        let mut map = v.cast::<Map>();
        let shader = match map.remove("shader") {
            Some(f) => Some(tracing.material(&f, "the floor shader")?),
            None => None,
        };
        let m = material(map).map_err(|e| format!("floor: {e}"))?;
        return Ok(Some((m, shader)));
    }
    let c = color(&v).map_err(|e| format!("floor: {e}, or true, false or a material map"))?;
    Ok(Some((Material::diffuse(c), None)))
}

/// The script's view: any of azimuth, elevation, fov, aperture, target, distance (degrees and
/// world units, as in `CameraParams`). Checked against the same limits the renderer enforces.
fn camera(mut map: Map) -> Result<crate::CameraHint, String> {
    let mut num = |key: &str, lo: f64, hi: f64| -> Result<Option<f64>, String> {
        match map.remove(key) {
            None => Ok(None),
            Some(v) => {
                let n = number(&v, key).map_err(|e| format!("camera: {e}"))?;
                if !(lo..=hi).contains(&n) {
                    return Err(format!("camera: `{key}` must be between {lo} and {hi}, got {n}"));
                }
                Ok(Some(n))
            }
        }
    };
    let hint = crate::CameraHint {
        azimuth: num("azimuth", -3600.0, 3600.0)?,
        elevation: num("elevation", -89.0, 89.0)?,
        fov: num("fov", 5.0, 120.0)?,
        aperture: num("aperture", 0.0, 1.0)?,
        distance: num("distance", 0.5, 100.0)?,
        target: match map.remove("target") {
            None => None,
            Some(t) => {
                let v = vec3(&t, "target").map_err(|e| format!("camera: {e}"))?;
                if v.iter().any(|c| c.abs() > 100.0) {
                    return Err("camera: `target` must be within 100 units of the origin".into());
                }
                Some([v.x, v.y, v.z])
            }
        },
    };
    if let Some(key) = map.keys().next() {
        return Err(format!(
            "camera: unknown field `{key}` (expected azimuth, elevation, fov, aperture, target, distance)"
        ));
    }
    Ok(hint)
}

fn volumes(value: Dynamic) -> Result<Vec<crate::volume::VolumeSpec>, String> {
    use crate::volume::{Temperature, VolumeSpec};
    if !value.is_array() {
        return Err(format!("`volumes` must be a list, got {}", value.type_name()));
    }
    let items = value.cast::<rhai::Array>();
    if items.len() > MAX_VOLUMES {
        return Err(format!("{} volumes given; at most {MAX_VOLUMES} are allowed", items.len()));
    }
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if !item.is_map() {
                return Err(format!("volume {i} must be a map such as #{{ density: ..., temperature: 1800 }}"));
            }
            let mut map = item.cast::<Map>();
            let density = map.remove("density").ok_or_else(|| format!("volume {i} is missing `density`"))?;
            let density = if density.is::<Tree>() {
                density.cast::<Tree>()
            } else {
                Tree::constant(number(&density, "density").map_err(|e| format!("volume {i}: {e}"))? as f32)
            };
            let temperature = match map.remove("temperature") {
                None => None,
                Some(t) if t.is::<Tree>() => Some(Temperature::Field(t.cast::<Tree>())),
                Some(t) => Some(Temperature::Constant(
                    number(&t, "temperature").map_err(|e| format!("volume {i}: {e}"))?.clamp(500.0, 12000.0),
                )),
            };
            // A volume's color is how much light it scatters per channel, read directly (no sRGB
            // decoding): "#999999" scatters 60%, which is how light grey smoke behaves. Decoding
            // it like a surface color would make it scatter 33% and look dark and thin.
            let albedo = match map.remove("color") {
                None => None,
                Some(c) => Some(albedo(&c).map_err(|e| format!("volume {i}: {e}"))?),
            };
            if temperature.is_none() && albedo.is_none() {
                return Err(format!(
                    "volume {i} needs `temperature` (glowing, like fire) and/or `color` (scattering, like smoke)"
                ));
            }
            let anisotropy = match map.remove("anisotropy") {
                None => 0.3,
                Some(g) => number(&g, "anisotropy").map_err(|e| format!("volume {i}: {e}"))?.clamp(-0.95, 0.95),
            };
            let brightness = match map.remove("brightness") {
                None => 1.0,
                Some(b) => number(&b, "brightness").map_err(|e| format!("volume {i}: {e}"))?.clamp(0.0, 100.0),
            };
            if let Some(key) = map.keys().next() {
                return Err(format!(
                    "volume {i}: unknown field `{key}` (expected density, temperature, color, anisotropy, brightness)"
                ));
            }
            Ok(VolumeSpec { density, temperature, albedo, anisotropy, brightness })
        })
        .collect()
}

fn lights(value: Dynamic) -> Result<Vec<Light>, String> {
    if !value.is_array() {
        return Err(format!("`lights` must be a list, got {}", value.type_name()));
    }
    let items = value.cast::<rhai::Array>();
    if items.len() > MAX_LIGHTS {
        return Err(format!("{} lights given; at most {MAX_LIGHTS} are allowed", items.len()));
    }
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            if !item.is_map() {
                return Err(format!("light {i} must be a map such as #{{ point: [x, y, z] }}"));
            }
            light(item.cast::<Map>()).map_err(|e| format!("light {i}: {e}"))
        })
        .collect()
}

fn vec3(v: &Dynamic, field: &str) -> Result<glm::DVec3, String> {
    let bad = || format!("`{field}` must be [x, y, z]");
    if !v.is_array() {
        return Err(bad());
    }
    let a = v.clone().cast::<rhai::Array>();
    if a.len() != 3 {
        return Err(bad());
    }
    let c = a.iter().map(|x| number(x, field)).collect::<Result<Vec<_>, _>>()?;
    if c.iter().any(|x| !x.is_finite() || x.abs() > 1000.0) {
        return Err(bad());
    }
    Ok(glm::vec3(c[0], c[1], c[2]))
}

/// One light: exactly one of `sphere` (area light), `point`, or `sun` (direction toward it).
fn light(mut map: Map) -> Result<Light, String> {
    let kinds = ["sphere", "point", "sun"].iter().filter(|k| map.contains_key(**k)).count();
    if kinds != 1 {
        return Err("give exactly one of `sphere: [x, y, z]`, `point: [x, y, z]` or `sun: [dx, dy, dz]`".into());
    }
    let color = match map.remove("color") {
        Some(c) => color(&c)?,
        None => glm::vec3(1.0, 1.0, 1.0),
    };
    let intensity = |map: &mut Map, default: f64| -> Result<f64, String> {
        Ok(match map.remove("intensity") {
            Some(v) => number(&v, "intensity")?.clamp(0.0, 1000.0),
            None => default,
        })
    };
    let light = if let Some(pos) = map.remove("sphere") {
        let center = vec3(&pos, "sphere")?;
        let radius = match map.remove("radius") {
            Some(r) => number(&r, "radius")?.clamp(0.05, 10.0),
            None => 1.0,
        };
        let emit = intensity(&mut map, 8.0)?;
        Light::Object(
            Object::new(sphere().scale(&glm::vec3(radius, radius, radius)).translate(&center))
                .material(Material::light(color, emit)),
        )
    } else if let Some(pos) = map.remove("point") {
        let p = vec3(&pos, "point")?;
        Light::Point(color * intensity(&mut map, 30.0)?, p)
    } else {
        let toward = vec3(&map.remove("sun").unwrap(), "sun")?;
        if toward.norm() == 0.0 {
            return Err("`sun` direction must not be [0, 0, 0]".into());
        }
        // rpt's directional light takes the direction the light travels.
        Light::Directional(color * intensity(&mut map, 2.0)?, -toward.normalize())
    };
    if let Some(key) = map.keys().next() {
        return Err(format!("unknown field `{key}` (expected sphere/point/sun, radius, color, intensity)"));
    }
    Ok(light)
}

/// Interpret a value as a list of parts.
pub fn from_dynamic(value: Dynamic, tracing: &Tracing) -> Result<Vec<Part>, String> {
    if value.is::<Tree>() {
        return Ok(vec![Part { tree: value.cast::<Tree>(), material: None, shader: None }]);
    }
    if value.is_map() {
        return Ok(vec![part(value.cast::<Map>(), 0, tracing)?]);
    }
    if value.is_array() {
        let items = value.cast::<rhai::Array>();
        if items.is_empty() {
            return Err("the script returned an empty list of parts".into());
        }
        if items.len() > MAX_PARTS {
            return Err(format!("the script returned {} parts; at most {MAX_PARTS} are allowed", items.len()));
        }
        return items
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                if item.is::<Tree>() {
                    Ok(Part { tree: item.cast::<Tree>(), material: None, shader: None })
                } else if item.is_map() {
                    part(item.cast::<Map>(), i, tracing)
                } else {
                    Err(format!("part {i} must be a shape or #{{ shape, material }}, got {}", item.type_name()))
                }
            })
            .collect();
    }
    Err(format!(
        "the script must end with a shape or a list of #{{ shape, material }} parts, got {}",
        value.type_name()
    ))
}

fn part(mut map: Map, i: usize, tracing: &Tracing) -> Result<Part, String> {
    let shape = map.remove("shape").ok_or_else(|| format!("part {i} is missing `shape`"))?;
    if !shape.is::<Tree>() {
        return Err(format!("part {i}: `shape` must be a shape, got {}", shape.type_name()));
    }
    let material = match map.remove("material") {
        Some(m) if m.is_map() => Some(material(m.cast::<Map>()).map_err(|e| format!("part {i} material: {e}"))?),
        Some(m) => return Err(format!("part {i}: `material` must be a map, got {}", m.type_name())),
        None => None,
    };
    let shader = match map.remove("shader") {
        Some(f) => Some(tracing.material(&f, &format!("part {i} shader"))?),
        None => None,
    };
    if let Some(key) = map.keys().next() {
        return Err(format!("part {i}: unknown field `{key}` (expected `shape`, `material` and `shader`)"));
    }
    Ok(Part { tree: shape.cast::<Tree>(), material, shader })
}

fn number(v: &Dynamic, field: &str) -> Result<f64, String> {
    v.as_float()
        .map(|f| f as f64)
        .or_else(|_| v.as_int().map(|i| i as f64))
        .map_err(|_| format!("`{field}` must be a number"))
}

/// "#rrggbb" as sRGB components in 0..1.
pub(crate) fn hex_rgb(s: &str) -> Option<[f64; 3]> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    let rgb = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)?;
    Some([16, 8, 0].map(|shift| ((rgb >> shift) & 0xff) as f64 / 255.0))
}

/// A color's sRGB components in 0..1, from "#rrggbb" or [r, g, b].
fn components(v: &Dynamic) -> Result<[f64; 3], String> {
    if let Ok(s) = v.clone().into_string() {
        return hex_rgb(&s).ok_or_else(|| format!("`color` \"{s}\" must look like \"#rrggbb\""));
    }
    if v.is_array() {
        let a = v.clone().cast::<rhai::Array>();
        if a.len() == 3 {
            let c = [0, 1, 2].map(|i| number(&a[i], "color"));
            if let [Ok(r), Ok(g), Ok(b)] = c {
                if [r, g, b].iter().all(|x| (0.0..=1.0).contains(x)) {
                    return Ok([r, g, b]);
                }
            }
        }
    }
    Err("`color` must be \"#rrggbb\" or [r, g, b] with components in 0..1".into())
}

/// A surface color: sRGB decoded to linear, as rpt's `hex_color` does.
fn color(v: &Dynamic) -> Result<Color, String> {
    let [r, g, b] = components(v)?.map(|c| c.powf(2.2));
    Ok(glm::vec3(r, g, b))
}

/// A color read as a fraction per channel, without sRGB decoding (for volume albedo).
fn albedo(v: &Dynamic) -> Result<Color, String> {
    let [r, g, b] = components(v)?;
    Ok(glm::vec3(r, g, b))
}

fn material(map: Map) -> Result<Material, String> {
    let mut m = crate::default_material();
    let (mut glass, mut emit) = (false, 0.0);
    for (key, v) in &map {
        match key.as_str() {
            "color" => m.color = color(v)?,
            "roughness" => m.roughness = number(v, "roughness")?.clamp(0.01, 1.0),
            "metallic" => m.metallic = number(v, "metallic")?.clamp(0.0, 1.0),
            "ior" => m.index = number(v, "ior")?.clamp(1.0, 3.0),
            "glass" => glass = v.as_bool().map_err(|_| "`glass` must be true or false".to_string())?,
            "emit" => emit = number(v, "emit")?.clamp(0.0, 100.0),
            other => {
                return Err(format!(
                    "unknown field `{other}` (expected color, roughness, metallic, glass, ior, emit)"
                ))
            }
        }
    }
    if glass {
        m.transparent = true;
        m.metallic = 0.0;
        if !map.contains_key("color") {
            m.color = glm::vec3(1.0, 1.0, 1.0);
        }
        if !map.contains_key("roughness") {
            m.roughness = 0.01;
        }
    }
    m.emittance = emit;
    Ok(m)
}
