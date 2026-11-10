//! The scene every tile-render task reconstructs.
//!
//! The geometry comes from a user-written Fidget (Rhai) script, rendered on a floor under an
//! area light. Each request carries the script, the camera and the quality.

mod implicit;
mod parts;
pub mod preview;
mod shader;
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
    /// Most paths traced per pixel (sampling is adaptive, see `tracer.rs`).
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
pub(crate) fn default_material() -> Material {
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

/// A scene ready to trace: rpt's scene, plus what rpt has no notion of.
struct Built {
    scene: Scene,
    volumes: Vec<volume::Volume>,
    /// Per object in `scene.objects` (the floor first), the shader that sets its material.
    shaders: Vec<Option<shader::Shader>>,
    /// Replaces `scene.environment` when the script shades the sky.
    sky: Option<shader::Shader>,
    /// The floor (object 0) is the black void: it absorbs everything.
    void_floor: bool,
    /// The default sky: it lights the scene, but the camera sees black where it would show.
    hidden_sky: bool,
}

/// Build the scene: the user's parts, lit by the script's lights (or, if it places none, an
/// overhead area light) under the script's sky, plus any glowing volumes. Unless the script asks
/// for a floor and a sky, the background is black: the floor at z = 0 absorbs all light (so it
/// still hides anything below it), and the default soft blue-grey sky lights the scene without
/// being seen.
fn build_scene(params: &RenderParams) -> Result<Built, String> {
    let spec = implicit::parse_scene(&params.shape)?;
    let volumes = spec.volumes.into_iter().filter_map(volume::Volume::new).collect();
    let sky = spec.sky_shader.as_ref().map(shader::Shader::new).transpose()?;
    let mut scene = Scene::new();
    let hidden_sky = spec.sky.is_none() && sky.is_none();
    scene.environment = Environment::Color(spec.sky.unwrap_or_else(|| hex_color(0x9DB4D6) * 0.25));
    let void_floor = spec.floor.is_none();
    let (floor, floor_shader) = spec.floor.unwrap_or_else(|| (Material::diffuse(glm::vec3(0.0, 0.0, 0.0)), None));
    scene.add(Object::new(plane(glm::vec3(0.0, 0.0, 1.0), 0.0)).material(floor));
    let mut shaders = vec![floor_shader.as_ref().map(shader::Shader::new).transpose()?];
    // Each part is its own marched shape with its own material.
    for part in spec.parts {
        let material = part.material.unwrap_or_else(default_material);
        scene.add(Object::new(implicit::ImplicitShape::new(part.tree)).material(material));
        shaders.push(part.shader.as_ref().map(shader::Shader::new).transpose()?);
    }
    match spec.lights {
        Some(lights) => lights.into_iter().for_each(|l| scene.add(l)),
        None => scene.add(Light::Object(
            Object::new(sphere().scale(&glm::vec3(2.0, 2.0, 2.0)).translate(&glm::vec3(1.2, -1.5, 9.0)))
                .material(Material::light(hex_color(0xFFFFFF), 8.0)),
        )),
    }
    Ok(Built { scene, volumes, shaders, sky, void_floor, hidden_sky })
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
    fn shaders_color_parts_and_sky() {
        // A sphere shaded red on its +x half and blue on its -x half, under a green sky shader,
        // seen from straight above: the left and right of the image differ, the corners are sky.
        let script = r##"
            fn halves(p, n) { #{ color: select(step(0.0, p.x), "#ff0000", "#0000ff"), roughness: 0.9 } }
            fn sky(d) { mix("#00ff00", "#00ff00", 0.5) }
            #{ parts: [ #{ shape: sphere([0, 0, 1], 1), shader: halves } ], sky: sky,
               lights: [ #{ sun: [0, 0, 1], intensity: 3 } ] }
        "##;
        assert!(validate_scene(script).unwrap().bounds.is_some());
        let mut p = params(script);
        p.width = 32;
        p.height = 32;
        p.samples = 4;
        p.camera = CameraParams { target: [0.0, 0.0, 1.0], azimuth: -90.0, elevation: 89.0, distance: 5.0, fov: 40.0, aperture: 0.0 };
        let px = render_tile(&p, &Tile { x: 0, y: 0, w: 32, h: 32 }).unwrap();
        let at = |x: usize, y: usize| px[(y * 32 + x) * 3..(y * 32 + x) * 3 + 3].iter().map(|&c| i32::from(c)).collect::<Vec<_>>();
        let (left, right) = (at(10, 16), at(22, 16));
        assert!(right[0] > right[2] + 40, "the +x half is red: {right:?}");
        assert!(left[2] > left[0] + 40, "the -x half is blue: {left:?}");

        // From a low angle, the top row is open sky.
        p.camera.elevation = 3.0;
        let px = render_tile(&p, &Tile { x: 0, y: 0, w: 32, h: 1 }).unwrap();
        let top = &px[16 * 3..16 * 3 + 3];
        assert!(top[1] > 200 && top[0] < 40 && top[2] < 40, "the sky is green: {top:?}");
    }

    /// Bump-mapped ridges, lit by a low sun, show as light and dark bands; the same surface
    /// without the bump is evenly lit.
    #[test]
    fn bumps_shade_like_ridges() {
        let row_spread = |shader: &str| {
            let script = format!(
                r##"fn s(p, n) {{ #{{ {shader} roughness: 0.9 }} }}
                    #{{ parts: [ #{{ shape: box(#{{ lower: [-3, -3, 0], upper: [3, 3, 0.5] }}), shader: s }} ],
                       lights: [ #{{ sun: [1, 0, 0.35], intensity: 3 }} ], sky: "#000000" }}"##
            );
            let mut p = params(&script);
            (p.width, p.height, p.samples, p.bounces) = (48, 1, 16, 1);
            p.camera = CameraParams { target: [0.0, 0.0, 0.5], azimuth: -90.0, elevation: 89.0, distance: 4.0, fov: 30.0, aperture: 0.0 };
            let px = render_tile(&p, &Tile { x: 0, y: 0, w: 48, h: 1 }).unwrap();
            let lum: Vec<i32> = px.chunks(3).map(|c| i32::from(c[1])).collect();
            lum.iter().max().unwrap() - lum.iter().min().unwrap()
        };
        let flat = row_spread("");
        let ridged = row_spread("bump: 0.04 * sin(12.0 * p.x),");
        assert!(ridged > flat + 60, "ridges {ridged} vs flat {flat}");
    }

    /// The shader examples from the model's prompt (app/src/describe.ts) parse and render.
    #[test]
    fn prompt_shader_examples_work() {
        let script = r##"
            fn marble(p, n) {
              let v = 0.5 + 0.5 * sin(5.0 * p.x + 2.0 * p.z + 1.5 * sin(3.0 * p.y + 0.7 * sin(5.0 * p.z)));
              #{ color: mix("#f2efe9", "#5c6470", smoothstep(0.65, 0.95, v)), roughness: 0.15 }
            }
            fn mossy(p, n) {
              let up = smoothstep(0.35, 0.75, n.z + 0.15 * sin(9.0 * p.x) * cos(8.0 * p.y));
              #{ color: select(up, "#4f7a2a", "#6f6a64"), roughness: mix(0.6, 0.95, up) }
            }
            fn checker(p, n) { let c = fract(0.5 * (floor(2.0 * p.x) + floor(2.0 * p.y))) * 2.0;
              #{ color: mix("#1a1a1a", "#eeeeee", c) } }
            fn dusk(d) { mix("#f2a65a", "#1b2a4a", smoothstep(-0.02, 0.45, d.z)) }
            fn hammered(p, n) {
              let d = sin(17.0 * p.x + 3.0 * sin(5.0 * p.z)) * sin(19.0 * p.y + 2.0 * sin(4.0 * p.x)) * sin(15.0 * p.z);
              #{ color: "#b87333", metallic: 1.0, roughness: 0.3, bump: 0.012 * d }
            }
            let c1 = "#aa3311";
            let c2 = [0.1, 0.2, 0.9];
            #{ parts: [
                 #{ shape: sphere([0, 0, 1], 1), shader: marble },
                 #{ shape: sphere([2.5, 0, 1], 1), shader: mossy, material: #{ metallic: 0.2 } },
                 #{ shape: box(#{ lower: [-4, -4, 0], upper: [4, -3, 0.2] }), shader: checker },
                 #{ shape: sphere([0, 2.5, 1], 1), shader: hammered },
                 #{ shape: sphere([-2.5, 0, 1], 1), shader: |p, n| #{ color: mix(c1, c2, step(0.0, p.x)) } },
               ], sky: dusk }
        "##;
        validate_scene(script).unwrap();
        assert!(render_tile(&params(script), &Tile { x: 0, y: 0, w: 16, h: 16 }).is_ok());
    }

    /// Unless a script asks for a floor or a sky, everything around the shape is pure black,
    /// while the hidden sky still lights it.
    #[test]
    fn the_default_background_is_black() {
        let render = |script: &str, elevation: f64| {
            let mut p = params(script);
            (p.width, p.height, p.samples) = (32, 32, 8);
            p.camera = CameraParams { target: [0.0, 0.0, 1.0], azimuth: -90.0, elevation, distance: 6.0, fov: 40.0, aperture: 0.0 };
            render_tile(&p, &Tile { x: 0, y: 0, w: 32, h: 32 }).unwrap()
        };
        let px = |img: &[u8], x: usize, y: usize| [0, 1, 2].map(|c| img[(y * 32 + x) * 3 + c]);
        let ball = "sphere([0, 0, 1], 1)";
        // Low view: the top corner is sky, the bottom corner floor, the center the ball.
        let img = render(ball, 5.0);
        assert_eq!(px(&img, 0, 0), [0, 0, 0], "sky");
        assert_eq!(px(&img, 0, 31), [0, 0, 0], "floor");
        assert!(px(&img, 16, 12).iter().all(|&c| c > 30), "the ball is lit: {:?}", px(&img, 16, 12));
        // A chrome ball facing the hidden sky still reflects it (not black).
        let chrome = r#"[#{ shape: sphere([0, 0, 1], 1), material: #{ metallic: 1.0, roughness: 0.05 } }]"#;
        assert!(px(&render(chrome, 30.0), 16, 13).iter().any(|&c| c > 5));
        // Asking for a floor and a sky shows them.
        let img = render(r##"#{ parts: sphere([0, 0, 1], 1), floor: "#cc2222", sky: "#2244aa" }"##, 5.0);
        let (sky, floor) = (px(&img, 0, 0), px(&img, 0, 31));
        assert!(sky[2] > sky[0] + 40, "blue sky: {sky:?}");
        assert!(floor[0] > floor[2] + 20, "red floor: {floor:?}");
        assert!(render(r#"#{ parts: sphere([0, 0, 1], 1), floor: true }"#, 5.0)[31 * 32 * 3] > 40, "the old grey floor");
        // Floors take material maps with shaders, and bad floors are explained.
        validate_scene(r##"fn tiles(p, n) { #{ color: mix("#111111", "#eeeeee", step(0.5, fract(p.x))) } }
            #{ parts: sphere(), floor: #{ roughness: 0.2, shader: tiles } }"##).unwrap();
        assert!(validate_shape("#{ parts: sphere(), floor: 3 }").unwrap_err().contains("floor:"));
    }

    /// Adaptive sampling stops smooth pixels early, and still closely matches a render allowed 4x the samples.
    #[test]
    fn adaptive_sampling_saves_work() {
        let script = r#"#{ parts: [ #{ shape: sphere([0, 0, 1], 1) },
            #{ shape: sphere([2.2, 0, 0.7], 0.7), material: #{ glass: true, ior: 1.5, roughness: 0.05 } } ],
            floor: true, lights: [ #{ point: [2, -3, 4], intensity: 30 } ] }"#;
        let mut p = params(script);
        (p.width, p.height, p.bounces) = (48, 32, 4);
        p.camera = CameraParams { target: [1.0, 0.0, 0.8], distance: 8.0, ..CameraParams::default() };
        let tile = Tile { x: 0, y: 0, w: 48, h: 32 };
        let render = |samples| {
            let p = RenderParams { samples, ..p.clone() };
            tracer::render_tile_counted(&p, &tile).unwrap()
        };
        let (img, traced) = render(256);
        let (reference, _) = render(1024);
        let full = 256 * 48 * 32;
        let err = img.iter().zip(&reference).map(|(&a, &b)| (f64::from(a) - f64::from(b)).abs()).sum::<f64>() / img.len() as f64;
        eprintln!("traced {traced} of {full} ({:.0}%), mean error {err:.2} levels", 100.0 * traced as f64 / full as f64);
        assert!(traced < full * 2 / 3, "traced {traced} of {full}");
        assert!(err < 2.5, "mean error {err}");
    }

    /// `at` places a part (and its shader's `p`), and labels are kept; the prompt's examples
    /// (app/src/describe.ts) that use them run.
    #[test]
    fn parts_are_placed_with_at() {
        let b = validate_shape(r#"[#{ label: "ball", shape: sphere([0, 0, 0], 1), at: [3, 0, 1] }]"#).unwrap().unwrap();
        assert!((b.min[0] - 2.0).abs() < 0.4 && (b.max[0] - 4.0).abs() < 0.4, "{b:?}");
        let spec = implicit::parse_scene(r#"[#{ label: "ball", shape: sphere(), at: [3, 0, 1] }]"#).unwrap();
        assert_eq!((spec.parts[0].label.as_deref(), spec.parts[0].at), (Some("ball"), [3.0, 0.0, 1.0]));
        assert!(validate_shape(r#"[#{ shape: sphere(), at: [1, 2] }]"#).unwrap_err().contains("[x, y, z]"));
        assert!(validate_shape(r#"[#{ shape: sphere(), label: 3 }]"#).unwrap_err().contains("label"));

        // A shader that colors the part's own +x half red moves with it.
        let script = |at: &str| format!(
            r##"[#{{ shape: sphere([0, 0, 0], 1), at: {at}, shader: |p, n| #{{ color: select(step(0.0, p.x), "#ff0000", "#0000ff") }} }}]"##
        );
        let shaded = |at: &str| {
            let spec = implicit::parse_scene(&script(at)).unwrap();
            let shader = shader::Shader::new(spec.parts[0].shader.as_ref().unwrap()).unwrap();
            let mut m = default_material();
            shader.shade(&mut m, &glm::vec3(2.5, 0.0, 1.0), &glm::vec3(1.0, 0.0, 0.0));
            m.color
        };
        assert!(shaded("[0, 0, 1]").x > 0.9, "x = 2.5 is on the +x side of a ball at the origin");
        assert!(shaded("[3, 0, 1]").z > 0.9, "x = 2.5 is on the -x side of a ball at x = 3");

        // `rotate` turns a part about its own origin, x first, then y, then z: a long thin box along
        // x, turned 90 degrees about z, lies along y.
        let b = validate_shape(r#"[#{ shape: box(#{ lower: [-2, -0.2, -0.2], upper: [2, 0.2, 0.2] }), rotate: [0, 0, 90], at: [0, 0, 1] }]"#).unwrap().unwrap();
        assert!(b.max[1] - b.min[1] > 3.5 && b.max[0] - b.min[0] < 1.0, "{b:?}");
        // The shape's surface lands where `to_world` puts it, for a turn about all three axes.
        let spec = implicit::parse_scene(r#"[#{ shape: box(#{ lower: [0, 0, 0], upper: [1, 2, 3] }), rotate: [30, -50, 70], at: [1, 2, 3] }]"#).unwrap();
        let part = &spec.parts[0];
        for corner in [[1.0, 2.0, 3.0], [1.0, 0.0, 0.0], [0.0, 2.0, 3.0]] {
            let w = parts::to_world(part.at, part.rotate, corner);
            let mut eval = fidget::vm::VmShape::new_point_eval();
            let shape = fidget::vm::VmShape::from(part.tree.clone());
            let tape = shape.point_tape(Default::default());
            let (v, _) = eval.eval(&tape, w[0] as f32, w[1] as f32, w[2] as f32).unwrap();
            assert!(v.abs() < 1e-4, "corner {corner:?} at {w:?}: {v}");
        }

        for example in [
            r##"let tower = extrude_z(circle([0, 0], 0.55), #{ lower: 0.4, upper: 3.4 });
                let stripe = intersection(tower.scale_uniform(1.02), box(#{ lower: [-1, -1, 1.6], upper: [1, 1, 2.1] }));
                let tower_at = [0.0, 0.0, 0.0];
                [ #{ label: "tower", shape: tower, at: tower_at, material: #{ color: "#ece6d8", roughness: 0.5 } },
                  #{ label: "stripe", shape: stripe, at: tower_at, material: #{ color: "#b3261e", roughness: 0.4 } } ]"##,
            r##"let post = extrude_z(circle([0, 0], 0.08), #{ lower: 0, upper: 2.2 });
                let glow = sphere([0, 0, 2.45], 0.2);
                let lantern_at = [0.0, 0.0, 0.0];
                #{ parts: [ #{ label: "post", shape: post, at: lantern_at }, #{ label: "glow", shape: glow, at: lantern_at } ],
                   lights: [ #{ point: [lantern_at[0] + 0.3, lantern_at[1] - 0.3, 2.45], color: "#ffcf87", intensity: 20 } ] }"##,
            r#"let lamp_at = [2, 0, 0]; #{ parts: sphere(), lights: [#{ point: [lamp_at[0], lamp_at[1], lamp_at[2] + 2.4] }] }"#,
        ] {
            validate_scene(example).unwrap();
        }
    }

    #[test]
    fn shader_comparisons_get_a_hint() {
        let err = validate_shape("fn s(p, n) { if n.z > 0.5 { #{} } else { #{} } } [#{ shape: sphere(), shader: s }]").unwrap_err();
        assert!(err.contains("use step, smoothstep or select"), "{err}");
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
