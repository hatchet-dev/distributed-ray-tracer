//! The page's live preview (built for the browser with wasm-bindgen): meshes each part of a
//! shape script with Fidget's mesher, and finds the `[x, y, z]` literal each labelled part's
//! `at` comes from, so dragging the part in the preview can rewrite exactly that text.

mod handles;
mod mesh;

use serde::Serialize;
use wasm_bindgen::prelude::*;

pub use handles::{find_handles, Handle, Placement};
pub use mesh::{mesh_part, PartMesh};

/// Default octree depth: 2^6 = 64 cells across each part's bounding box.
pub const DEFAULT_DEPTH: u8 = 6;

/// A script's preview: JSON metadata (see [`Meta`]) and one mesh per part.
#[wasm_bindgen]
pub struct Preview {
    meta: String,
    meshes: Vec<PartMesh>,
}

#[wasm_bindgen]
impl Preview {
    /// JSON: `{ parts, handles, volumes, floor, sky, camera }`, or `{ error }`.
    pub fn meta(&self) -> String {
        self.meta.clone()
    }
    /// Part `i`'s vertex positions (x, y, z per vertex), around the part's own origin.
    pub fn positions(&self, i: usize) -> Vec<f32> {
        self.meshes.get(i).map(|m| m.positions.clone()).unwrap_or_default()
    }
    pub fn normals(&self, i: usize) -> Vec<f32> {
        self.meshes.get(i).map(|m| m.normals.clone()).unwrap_or_default()
    }
    /// Linear RGB per vertex.
    pub fn colors(&self, i: usize) -> Vec<f32> {
        self.meshes.get(i).map(|m| m.colors.clone()).unwrap_or_default()
    }
    pub fn indices(&self, i: usize) -> Vec<u32> {
        self.meshes.get(i).map(|m| m.indices.clone()).unwrap_or_default()
    }
}

#[derive(Serialize)]
struct Meta<'a> {
    parts: Vec<PartMeta<'a>>,
    handles: Vec<Handle>,
    volumes: &'a [scene::Bounds],
    floor: Option<[f32; 3]>,
    sky: Option<[f32; 3]>,
    camera: Option<scene::CameraHint>,
}

#[derive(Serialize)]
struct PartMeta<'a> {
    label: Option<&'a str>,
    at: [f64; 3],
    /// Degrees, as in the script: about x, then y, then z (three.js Euler order "ZYX").
    rotate: [f64; 3],
    /// Where `at` and `rotate` are written, if the part can be moved.
    placed: Placement,
    color: [f32; 3],
    roughness: f32,
    metallic: f32,
    glass: bool,
    emit: f32,
}

/// Runs and meshes `script`, `depth` octree levels deep (0: the default).
#[wasm_bindgen]
pub fn preview(script: &str, depth: u8) -> Preview {
    let depth = if depth == 0 { DEFAULT_DEPTH } else { depth.min(8) };
    match build(script, depth) {
        Ok(p) => p,
        Err(error) => Preview { meta: serde_json::json!({ "error": error }).to_string(), meshes: vec![] },
    }
}

fn build(script: &str, depth: u8) -> Result<Preview, String> {
    let scene = scene::preview::preview_scene(script)?;
    let labels: Vec<_> = scene.parts.iter().map(|p| p.label.as_deref()).collect();
    let ats: Vec<_> = scene.parts.iter().map(|p| p.at).collect();
    let rotates: Vec<_> = scene.parts.iter().map(|p| p.rotate).collect();
    let (handles, placements) = find_handles(script, &labels, &ats, &rotates);
    let meshes = scene.parts.iter().map(|p| mesh_part(p, depth)).collect();
    let meta = Meta {
        parts: scene
            .parts
            .iter()
            .zip(placements)
            .map(|(p, placed)| PartMeta {
                label: p.label.as_deref(),
                at: p.at,
                rotate: p.rotate,
                placed,
                color: p.material.color,
                roughness: p.material.roughness,
                metallic: p.material.metallic,
                glass: p.material.glass,
                emit: p.material.emit,
            })
            .collect(),
        handles,
        volumes: &scene.volumes,
        floor: scene.floor,
        sky: scene.sky,
        camera: scene.camera,
    };
    Ok(Preview { meta: serde_json::to_string(&meta).map_err(|e| e.to_string())?, meshes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meshes_parts_around_their_origin() {
        let script = r##"fn tint(p, n) { #{ color: mix("#000000", "#ffffff", step(0.0, n.z)) } }
            #{ parts: [ #{ label: "ball", shape: sphere([0, 0, 0], 1), at: [2, 0, 1], shader: tint } ] }"##;
        let p = preview(script, 0);
        let meta: serde_json::Value = serde_json::from_str(&p.meta()).unwrap();
        assert_eq!(meta["parts"][0]["placed"]["at"], 0, "{meta}");
        let pos = p.positions(0);
        assert!(pos.len() > 300);
        // Around its own origin: a unit sphere.
        for v in pos.chunks(3) {
            let r = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            assert!((r - 1.0).abs() < 0.05, "radius {r}");
        }
        // Normals point outward, and the shader sees them: white on top, black below.
        let (normals, colors) = (p.normals(0), p.colors(0));
        for ((v, n), c) in pos.chunks(3).zip(normals.chunks(3)).zip(colors.chunks(3)) {
            assert!(v[0] * n[0] + v[1] * n[1] + v[2] * n[2] > 0.5, "v {v:?} n {n:?}");
            if v[2] > 0.2 { assert!(c[0] > 0.9) }
            if v[2] < -0.2 { assert!(c[0] < 0.1) }
        }
        assert!(p.indices(0).len() % 3 == 0);
    }

    #[test]
    fn mesh_timing() {
        let script = r#"let g = abs(sin(3 * x) * cos(3 * y) + sin(3 * y) * cos(3 * z) + sin(3 * z) * cos(3 * x)) / 3 - 0.08;
            [ #{ label: "gyroid", shape: intersection(g, sphere([0, 0, 1.5], 1.5)), at: [0, 0, 0] },
              #{ label: "box", shape: box(#{ lower: [-1, -1, 0], upper: [1, 1, 0.3] }) } ]"#;
        for depth in [5, 6, 7] {
            let t = std::time::Instant::now();
            let p = preview(script, depth);
            eprintln!("depth {depth}: {:?}, {} + {} triangles", t.elapsed(), p.indices(0).len() / 3, p.indices(1).len() / 3);
        }
    }
}

