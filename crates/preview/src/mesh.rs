//! Meshing one part with Fidget's octree mesher (dual contouring).

use fidget::{
    mesh::{Octree, Settings},
    vm::VmShape,
};
use scene::preview::PreviewPart;

/// A part's triangle mesh, flattened for WebGL buffers.
#[derive(Default)]
pub struct PartMesh {
    pub positions: Vec<f32>,
    pub normals: Vec<f32>,
    pub colors: Vec<f32>,
    pub indices: Vec<u32>,
}

/// Meshes `part` around its own origin, over its bounding box (padded, as a cube), with
/// `2^depth` cells across. Empty if the part has no surface.
pub fn mesh_part(part: &PreviewPart, depth: u8) -> PartMesh {
    let Some(b) = part.bounds else { return PartMesh::default() };
    let center = nalgebra::Vector3::from_fn(|i, _| 0.5 * (b.min[i] + b.max[i]));
    let half = (0..3).map(|i| 0.5 * (b.max[i] - b.min[i])).fold(0.0f32, f32::max) * 1.05 + 1e-3;
    let settings = Settings {
        depth,
        // The mesher works in [-1, 1]^3; this maps that cube onto the part's box.
        world_to_model: nalgebra::Matrix4::new_translation(&center) * nalgebra::Matrix4::new_scaling(half),
        threads: None,
        ..Default::default()
    };
    let Ok(shape) = VmShape::from(part.tree.clone()).try_into() else { return PartMesh::default() };
    let Some(octree) = Octree::build(&shape, &settings) else { return PartMesh::default() };
    let mesh = octree.walk_dual();

    // Face normals (length = twice the area, to weight by area), and each vertex's faces.
    let faces: Vec<_> = mesh
        .triangles
        .iter()
        .map(|t| {
            let [a, b, c] = [t.x, t.y, t.z].map(|i| mesh.vertices[i]);
            (b - a).cross(&(c - a))
        })
        .collect();
    let mut around = vec![vec![]; mesh.vertices.len()];
    for (f, t) in mesh.triangles.iter().enumerate() {
        for i in [t.x, t.y, t.z] {
            around[i].push(f);
        }
    }
    // Each corner's normal averages the faces around its vertex that bend less than CREASE from
    // its own face, so smooth surfaces shade smoothly and edges (a box's) stay sharp. Corners
    // with the same vertex and normal share an output vertex.
    let mut out = PartMesh::default();
    let mut index = std::collections::HashMap::new();
    for (f, t) in mesh.triangles.iter().enumerate() {
        for v in [t.x, t.y, t.z] {
            // The vertex's average normal. The mesher occasionally folds a triangle over (wound
            // backwards), so each face counts with whichever side agrees with it.
            let smooth: nalgebra::Vector3<f32> = around[v].iter().map(|&g| faces[g]).sum();
            let facing = |g: nalgebra::Vector3<f32>| if g.dot(&smooth) < 0.0 { -g } else { g };
            // A sliver's own normal is unreliable: bend from the average instead.
            let largest = around[v].iter().map(|&g| faces[g].norm()).fold(0.0, f32::max);
            let own = if faces[f].norm() < 0.05 * largest { smooth } else { facing(faces[f]) };
            let own = own.try_normalize(1e-20).unwrap_or(nalgebra::Vector3::z());
            let n: nalgebra::Vector3<f32> = around[v]
                .iter()
                .map(|&g| facing(faces[g]))
                .filter(|g| g.try_normalize(1e-20).is_some_and(|g| g.dot(&own) > CREASE))
                .sum();
            let n = n.try_normalize(1e-20).unwrap_or(own);
            let key = (v, n.map(|c| (c * 1000.0).round() as i32));
            let i = *index.entry(key).or_insert_with(|| {
                let p = mesh.vertices[v];
                out.positions.extend([p.x, p.y, p.z]);
                out.normals.extend([n.x, n.y, n.z]);
                out.colors.extend(part.color_at([p.x, p.y, p.z], [n.x, n.y, n.z]));
                (out.positions.len() / 3 - 1) as u32
            });
            out.indices.push(i);
        }
    }
    out
}

/// Faces meeting at more than this angle (cos 40°) form a sharp edge.
const CREASE: f32 = 0.766;
