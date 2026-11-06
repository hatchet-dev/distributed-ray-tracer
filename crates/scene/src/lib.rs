//! The scene every tile-render task reconstructs.
//!
//! The geometry comes from a user-written Fidget (Rhai) script, rendered on a floor under an
//! area light. Each request carries the script, the camera and the quality.

mod implicit;
mod parts;
mod tracer;
mod volume;

use rpt::*;
use serde::{Deserialize, Serialize};
pub use tracer::render_tile;

/// Longest accepted shape script, in bytes.
/// Fits in one Hatchet durable stream message (just under 4 MiB), where scripts are stored.
pub const MAX_SCRIPT_BYTES: usize = 3 * 1024 * 1024;

/// A camera chosen by a script: any subset of the orbit camera's fields. Fields left out are
/// kept from the current view (angles) or auto-framed on the shape (target, distance).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct CameraHint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azimuth: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fov: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aperture: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
}

/// What validating a script reports: the visible bounding box and any camera it chose.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Validation {
    pub bounds: Option<Bounds>,
    pub camera: Option<CameraHint>,
}

/// Bounding box of a shape: `min` and `max` corners in world units.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// Check that a shape script runs and yields a valid scene; report the bounding box of
/// everything visible (for framing the camera) and the script's camera, if it set one.
pub fn validate_scene(script: &str) -> Result<Validation, String> {
    if script.len() > MAX_SCRIPT_BYTES {
        return Err(format!("shape script is longer than {MAX_SCRIPT_BYTES} bytes"));
    }
    let spec = implicit::parse_scene(script)?;
    let mut total: Option<Bounds> = None;
    let boxes = spec
        .parts
        .into_iter()
        .filter_map(|part| implicit::bounds(part.tree))
        .chain(spec.volumes.iter().filter_map(|v| volume::density_bounds(&v.density)));
    for (min, max) in boxes {
        let b = total.get_or_insert(Bounds { min, max });
        for i in 0..3 {
            b.min[i] = b.min[i].min(min[i]);
            b.max[i] = b.max[i].max(max[i]);
        }
    }
    Ok(Validation { bounds: total, camera: spec.camera })
}

/// The bounding box [`validate_scene`] reports (`None` if nothing is visible).
#[cfg(test)]
fn validate_shape(script: &str) -> Result<Option<Bounds>, String> {
    validate_scene(script).map(|v| v.bounds)
}

/// Render settings, identical for every tile of one image.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenderParams {
    pub width: u32,
    pub height: u32,
    /// Paths traced per pixel.
    pub samples: u32,
    /// Maximum bounces per path.
    pub bounces: u32,
    #[serde(default)]
    pub camera: CameraParams,
    /// Rhai script whose final expression is the shape (Fidget's scripting language). Tasks
    /// carry a script id instead, and the endpoint fills this in.
    #[serde(default)]
    pub shape: String,
}

/// The material of a part whose script gives it none: warm clay.
fn default_material() -> Material {
    Material::specular(hex_color(0xD9C2A7), 0.35)
}

/// A rectangle of pixels; `(x, y)` is its top-left corner.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl RenderParams {
    pub fn validate(&self, tile: &Tile) -> Result<(), String> {
        if !(1..=7680).contains(&self.width) || !(1..=4320).contains(&self.height) {
            return Err("image must be at most 7680x4320".into());
        }
        if !(1..=4096).contains(&self.samples) || self.bounces > 16 {
            return Err("samples must be in 1..=4096 and bounces in 0..=16".into());
        }
        if tile.w == 0 || tile.h == 0 || tile.w * tile.h > 256 * 256 {
            return Err("tile must be non-empty and at most 256x256 pixels".into());
        }
        if tile.x + tile.w > self.width || tile.y + tile.h > self.height {
            return Err("tile must lie inside the image".into());
        }
        if self.shape.len() > MAX_SCRIPT_BYTES {
            return Err(format!("shape script is longer than {MAX_SCRIPT_BYTES} bytes"));
        }
        self.camera.validate()
    }
}

/// An orbit camera: it sits on a sphere around `target` and looks at it.
/// The world is Z-up; angles are in degrees. Depth of field focuses on the target.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraParams {
    pub target: [f64; 3],
    /// Angle around the Z axis, from +X towards +Y.
    pub azimuth: f64,
    /// Angle above the ground plane.
    pub elevation: f64,
    pub distance: f64,
    /// Field of view across the longer image dimension.
    pub fov: f64,
    /// Lens radius (0 = pinhole).
    pub aperture: f64,
}

impl Default for CameraParams {
    fn default() -> Self {
        Self {
            target: [0.0, 0.0, 1.2],
            azimuth: -60.0,
            elevation: 18.0,
            distance: 9.0,
            fov: 39.6,
            aperture: 0.0,
        }
    }
}

impl CameraParams {
    fn validate(&self) -> Result<(), String> {
        let ok = self.target.iter().all(|v| v.is_finite() && v.abs() <= 100.0)
            && self.azimuth.is_finite()
            && (-89.0..=89.0).contains(&self.elevation)
            && (0.5..=100.0).contains(&self.distance)
            && (5.0..=120.0).contains(&self.fov)
            && (0.0..=1.0).contains(&self.aperture);
        ok.then_some(()).ok_or_else(|| "camera parameters out of range".into())
    }

    fn eye(&self) -> glm::DVec3 {
        let (az, el) = (self.azimuth.to_radians(), self.elevation.to_radians());
        glm::DVec3::from(self.target)
            + glm::vec3(el.cos() * az.cos(), el.cos() * az.sin(), el.sin()) * self.distance
    }

    fn camera(&self) -> Camera {
        let target = glm::DVec3::from(self.target);
        let camera = Camera::look_at(self.eye(), target, glm::vec3(0.0, 0.0, 1.0), self.fov.to_radians());
        if self.aperture > 0.0 {
            camera.focus(target, self.aperture)
        } else {
            camera
        }
    }
}

/// Build the scene: the user's parts on a floor, lit by the script's lights (or, if it places
/// none, an overhead area light) under the script's sky (or a soft blue-grey one), plus any
/// glowing volumes.
fn build_scene(params: &RenderParams) -> Result<(Scene, Vec<volume::Volume>), String> {
    let spec = implicit::parse_scene(&params.shape)?;
    let volumes = spec.volumes.into_iter().filter_map(volume::Volume::new).collect();
    let mut scene = Scene::new();
    scene.environment = Environment::Color(spec.sky.unwrap_or_else(|| hex_color(0x9DB4D6) * 0.25));
    scene.add(
        Object::new(plane(glm::vec3(0.0, 0.0, 1.0), 0.0))
            .material(Material::diffuse(hex_color(0xE7E7E7))),
    );
    // Each part is its own marched shape with its own material.
    for part in spec.parts {
        let material = part.material.unwrap_or_else(default_material);
        scene.add(Object::new(implicit::ImplicitShape::new(part.tree)).material(material));
    }
    match spec.lights {
        Some(lights) => lights.into_iter().for_each(|l| scene.add(l)),
        None => scene.add(Light::Object(
            Object::new(sphere().scale(&glm::vec3(2.0, 2.0, 2.0)).translate(&glm::vec3(1.2, -1.5, 9.0)))
                .material(Material::light(hex_color(0xFFFFFF), 8.0)),
        )),
    }
    Ok((scene, volumes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(shape: &str) -> RenderParams {
        RenderParams {
            width: 16,
            height: 16,
            samples: 2,
            bounces: 2,
            camera: CameraParams::default(),
            shape: shape.into(),
        }
    }

    #[test]
    fn tiles_are_deterministic() {
        let p = params("sphere([0, 0, 1.2], 1.2)");
        let t = Tile { x: 4, y: 4, w: 8, h: 8 };
        let a = render_tile(&p, &t).unwrap();
        assert_eq!(a.len(), 8 * 8 * 3);
        assert_eq!(a, render_tile(&p, &t).unwrap());
    }

    #[test]
    fn bounds_contain_the_shape() {
        let b = validate_shape("sphere([1, 0, 2], 1)").unwrap().unwrap();
        for i in 0..3 {
            let (c, r) = ([1.0, 0.0, 2.0][i], 1.0);
            assert!(b.min[i] <= c - r && b.min[i] > c - r - 0.7, "min {:?}", b);
            assert!(b.max[i] >= c + r && b.max[i] < c + r + 0.7, "max {:?}", b);
        }
        // Entirely below the floor: nothing visible.
        assert_eq!(validate_shape("sphere([0, 0, -5], 1)").unwrap(), None);
    }

    #[test]
    fn parts_with_materials() {
        let script = r##"[
            #{ shape: sphere([0, 0, 1], 1), material: #{ color: "#ff0000", roughness: 0.2 } },
            #{ shape: sphere([3, 0, 1], 1), material: #{ glass: true, ior: 1.4 } },
            #{ shape: sphere([-3, 0, 1], 0.5), material: #{ color: [1.0, 0.8, 0.2], emit: 4 } },
            sphere([0, 3, 1], 0.5),
        ]"##;
        let parts = implicit::parse(script).unwrap();
        assert_eq!(parts.len(), 4);
        let red = parts[0].material.unwrap();
        assert!(red.color.x > 0.9 && red.color.y < 0.01 && (red.roughness - 0.2).abs() < 1e-9);
        let glass = parts[1].material.unwrap();
        assert!(glass.transparent && (glass.index - 1.4).abs() < 1e-9);
        assert_eq!(parts[2].material.unwrap().emittance, 4.0);
        assert!(parts[3].material.is_none(), "a bare shape uses the default material");

        // Bounds cover every part.
        let b = validate_shape(script).unwrap().unwrap();
        assert!(b.min[0] <= -3.5 && b.max[0] >= 4.0 && b.max[1] >= 3.5, "{b:?}");

        // Renders.
        let mut p = params(script);
        p.width = 8;
        p.height = 8;
        assert!(render_tile(&p, &Tile { x: 0, y: 0, w: 8, h: 8 }).is_ok());
    }

    #[test]
    fn scripts_can_place_lights_and_sky() {
        let script = r##"#{
            parts: [ #{ shape: sphere([0, 0, 1], 1), material: #{ color: "#3366cc" } } ],
            lights: [
                #{ sphere: [2, -3, 7], radius: 1.5, color: "#fff4e0", intensity: 10 },
                #{ point: [0, 0, 3], color: "#ffd27a", intensity: 4 },
                #{ sun: [-1, -2, 3], intensity: 1.5 },
            ],
            sky: "#0b1530",
        }"##;
        let spec = implicit::parse_scene(script).unwrap();
        assert_eq!(spec.parts.len(), 1);
        let lights = spec.lights.unwrap();
        assert!(matches!(lights[0], Light::Object(_)));
        assert!(matches!(lights[1], Light::Point(..)));
        match &lights[2] {
            // Points away from the sun.
            Light::Directional(_, d) => assert!(d.z < 0.0),
            _ => panic!("expected a sun"),
        }
        assert!(spec.sky.unwrap().z > spec.sky.unwrap().x);

        // A plain shape keeps the default light and sky.
        let plain = implicit::parse_scene("sphere([0, 0, 1], 1)").unwrap();
        assert!(plain.lights.is_none() && plain.sky.is_none());

        // Renders, and bounds come from the parts only.
        let mut p = params(script);
        p.width = 8;
        p.height = 8;
        assert!(render_tile(&p, &Tile { x: 0, y: 0, w: 8, h: 8 }).is_ok());
        let b = validate_shape(script).unwrap().unwrap();
        assert!(b.max[2] < 3.0, "lights don't affect framing: {b:?}");
    }

    #[test]
    fn scripts_can_choose_the_camera() {
        let v = validate_scene("#{ parts: sphere([0, 0, 1], 1), camera: #{ azimuth: 30, elevation: -5, fov: 60 } }").unwrap();
        let cam = v.camera.unwrap();
        assert_eq!((cam.azimuth, cam.elevation, cam.fov), (Some(30.0), Some(-5.0), Some(60.0)));
        assert!(cam.target.is_none() && cam.distance.is_none(), "unset fields stay unset (auto-framed)");
        assert!(v.bounds.is_some());
        assert_eq!(validate_scene("sphere([0, 0, 1], 1)").unwrap().camera, None);
        let err = validate_scene("#{ parts: sphere(), camera: #{ elevation: 120 } }").unwrap_err();
        assert!(err.contains("`elevation` must be between"), "{err}");
        let err = validate_scene("#{ parts: sphere(), camera: #{ zoom: 2 } }").unwrap_err();
        assert!(err.contains("unknown field `zoom`"), "{err}");
    }

    #[test]
    fn bad_lights_are_reported() {
        let err = |s: &str| validate_shape(s).unwrap_err();
        assert!(err("#{ parts: sphere(), lights: [#{ point: [0, 0, 3], sun: [0, 0, 1] }] }").contains("exactly one of"));
        assert!(err("#{ parts: sphere(), lights: [#{ point: [0, 0] }] }").contains("[x, y, z]"));
        assert!(err("#{ parts: sphere(), lights: [#{ point: [0, 0, 3], power: 3 }] }").contains("unknown field `power`"));
        assert!(err("#{ parts: sphere(), skybox: \"#000000\" }").contains("unknown scene field `skybox`"));
        assert!(err("#{ lights: [] }").contains("missing `parts`"));
        let many = format!("#{{ parts: sphere(), lights: [{}] }}", vec!["#{ point: [0, 0, 3] }"; 9].join(", "));
        assert!(err(&many).contains("at most 8"));
    }

    #[test]
    fn bad_materials_are_reported() {
        let err = |s: &str| validate_shape(s).unwrap_err();
        assert!(err(r##"[#{ shape: sphere(), material: #{ colour: "#fff" } }]"##).contains("unknown field `colour`"));
        assert!(err(r##"[#{ shape: sphere(), material: #{ color: "#ffff" } }]"##).contains("#rrggbb"));
        assert!(err(r#"[#{ shap: sphere() }]"#).contains("missing `shape`"));
        assert!(err("42").contains("must end with a shape"));
        let many = format!("[{}]", vec!["sphere()"; 17].join(", "));
        assert!(err(&many).contains("at most 16"));
    }

    #[test]
    fn bad_scripts_are_reported() {
        assert!(validate_shape("sphere([0, 0, 1], 1)").is_ok());
        let err = validate_shape("sphere([0, 0, 1], ").unwrap_err();
        assert!(err.starts_with("shape script:"), "{err}");
        assert!(validate_shape("loop {}").is_err(), "infinite loops hit Fidget's step limit");
        let long = "let n = 0; for i in 0..200000 { n += 1; } sphere([0, 0, 1], 1)";
        assert!(validate_shape(long).is_ok(), "scripts may run well past Fidget's default 50,000 steps");
    }
}
