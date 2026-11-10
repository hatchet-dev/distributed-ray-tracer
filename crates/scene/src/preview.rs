//! What the in-browser preview needs from a script: each part's shape around its own origin, its
//! placement and label, and its material, which the preview crate (crates/preview) meshes and
//! shows in three.js. Colors here are linear, as three.js works in.

use fidget::context::Tree;
use rpt::{glm, Material};

use crate::{implicit, shader::Shader, Bounds, CameraHint};

pub struct PreviewScene {
    pub parts: Vec<PreviewPart>,
    /// Bounding boxes of the glowing or smoky volumes.
    pub volumes: Vec<Bounds>,
    /// The floor's color, if the script shows one.
    pub floor: Option<[f32; 3]>,
    /// The sky's color, if the script shows one (a sky shader is sampled straight up).
    pub sky: Option<[f32; 3]>,
    pub camera: Option<CameraHint>,
}

pub struct PreviewPart {
    pub label: Option<String>,
    /// Where the part is placed and how it's turned (degrees); `tree` is the shape before both.
    pub at: [f64; 3],
    pub rotate: [f64; 3],
    pub tree: Tree,
    /// The shape's extent around its own origin (before `at`), below the floor too.
    pub bounds: Option<Bounds>,
    pub material: Look,
    shader: Option<Shader>,
}

/// A material, as a preview shows it.
#[derive(Clone, Copy, Debug)]
pub struct Look {
    pub color: [f32; 3],
    pub roughness: f32,
    pub metallic: f32,
    pub glass: bool,
    pub emit: f32,
}

impl From<&Material> for Look {
    fn from(m: &Material) -> Self {
        Look {
            color: [m.color.x as f32, m.color.y as f32, m.color.z as f32],
            roughness: m.roughness as f32,
            metallic: m.metallic as f32,
            glass: m.transparent,
            emit: m.emittance as f32,
        }
    }
}

impl PreviewPart {
    /// Whether the part's color varies over its surface (it has a shader).
    pub fn shaded(&self) -> bool {
        self.shader.is_some()
    }

    /// The part's color at a point on its surface, given in its own coordinates (as meshed from
    /// `tree`), with the unit normal there (also in its own coordinates).
    pub fn color_at(&self, local: [f32; 3], normal: [f32; 3]) -> [f32; 3] {
        let Some(shader) = &self.shader else { return self.material.color };
        let mut m = crate::default_material();
        let c = self.material.color;
        m.color = glm::vec3(c[0].into(), c[1].into(), c[2].into());
        let p = crate::parts::to_world(self.at, self.rotate, local.map(f64::from));
        let n = crate::parts::to_world([0.0; 3], self.rotate, normal.map(f64::from));
        let (p, n) = (glm::vec3(p[0], p[1], p[2]), glm::vec3(n[0], n[1], n[2]));
        shader.shade(&mut m, &p, &n);
        [m.color.x as f32, m.color.y as f32, m.color.z as f32]
    }
}

/// Runs a script for the preview.
pub fn preview_scene(script: &str) -> Result<PreviewScene, String> {
    if script.len() > crate::MAX_SCRIPT_BYTES {
        return Err(format!("shape script is longer than {} bytes", crate::MAX_SCRIPT_BYTES));
    }
    let spec = implicit::parse_scene(script)?;
    let bounds = |(min, max): ([f32; 3], [f32; 3])| Bounds { min, max };
    let parts = spec
        .parts
        .into_iter()
        .map(|part| {
            let material = part.material.unwrap_or_else(crate::default_material);
            Ok(PreviewPart {
                label: part.label,
                at: part.at,
                rotate: part.rotate,
                bounds: implicit::full_bounds(part.local.clone()).map(bounds),
                tree: part.local,
                material: Look::from(&material),
                shader: part.shader.as_ref().map(Shader::new).transpose()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let rgb = |c: &rpt::Color| [c.x as f32, c.y as f32, c.z as f32];
    let sky = match (&spec.sky, &spec.sky_shader) {
        (Some(c), _) => Some(rgb(c)),
        (None, Some(s)) => Shader::new(s)?.sky(&glm::vec3(0.0, 0.0, 1.0)).map(|c| rgb(&c)),
        (None, None) => None,
    };
    Ok(PreviewScene {
        parts,
        volumes: spec.volumes.iter().filter_map(|v| crate::volume::density_bounds(&v.density)).map(bounds).collect(),
        floor: spec.floor.map(|(m, _)| rgb(&m.color)),
        sky,
        camera: spec.camera,
    })
}
