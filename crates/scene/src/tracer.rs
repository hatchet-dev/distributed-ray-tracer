//! Renders an arbitrary rectangle of the image.
//!
//! rpt's `Renderer` only renders whole frames, and its per-pixel functions are private, so
//! this ports its integrator (`get_color` / `trace_ray` / `sample_lights` from
//! rpt 0.2.1's renderer.rs, MIT, (c) Eric Zhang and Alexander Morozov) on top of rpt's
//! public scene, camera, material and light types. Pixel coordinates match rpt's, so tiles
//! stitch into exactly the frame `Renderer::render` would frame.

use rand::{rngs::StdRng, Rng, SeedableRng};
use rpt::{color_bytes, glm, Color, HitRecord, Light, Material, Object, Ray, Scene, Shape};

use crate::{
    volume::{phase_hg, sample_hg, Event, Volume},
    RenderParams, Tile,
};

const EPSILON: f64 = 1e-12;
const FIREFLY_CLAMP: f64 = 100.0;
/// Cap on one light's direct contribution at a scattering point in smoke. A point light's
/// intensity grows as 1/distance², so scattering points sampled right next to one would
/// otherwise show up as isolated bright specks ("fireflies") all over the smoke.
const DIRECT_SCATTER_CLAMP: f64 = 8.0;

/// Render `tile` as packed sRGB bytes (row-major, `w * h * 3`); `Err` if the shape script is invalid.
pub fn render_tile(params: &RenderParams, tile: &Tile) -> Result<Vec<u8>, String> {
    let (scene, volumes) = crate::build_scene(params)?;
    let camera = params.camera.camera();
    let tracer = Tracer { scene: &scene, volumes: &volumes, max_bounces: params.bounces };
    // Seeded from the tile position, so a retried tile renders identically.
    let mut rng = StdRng::seed_from_u64((u64::from(tile.y) << 32) | u64::from(tile.x));

    let (width, height) = (params.width, params.height);
    let dim = width.max(height) as f64;
    let mut out = Vec::with_capacity((tile.w * tile.h * 3) as usize);
    for y in tile.y..tile.y + tile.h {
        for x in tile.x..tile.x + tile.w {
            // Normalized to rpt's [-1, 1] box over the longer dimension.
            let xn = ((2 * x + 1) as f64 - width as f64) / dim;
            let yn = ((2 * (height - y) - 1) as f64 - height as f64) / dim;
            let mut color = glm::vec3(0.0, 0.0, 0.0);
            for _ in 0..params.samples {
                let dx = rng.gen_range((-1.0 / dim)..(1.0 / dim));
                let dy = rng.gen_range((-1.0 / dim)..(1.0 / dim));
                color += tracer.trace(camera.cast_ray(xn + dx, yn + dy, &mut rng), 0, &mut rng);
            }
            out.extend(color_bytes(&(color / f64::from(params.samples))));
        }
    }
    Ok(out)
}

struct Tracer<'a> {
    scene: &'a Scene,
    /// Glowing, absorbing gas (fire); see `volume.rs`.
    volumes: &'a [Volume],
    max_bounces: u32,
}

impl Tracer<'_> {
    /// Monte Carlo estimate of the radiance along `ray`.
    fn trace(&self, ray: Ray, bounces: u32, rng: &mut StdRng) -> Color {
        let hit = self.closest_hit(ray);
        // Volumes in front of the surface (or the sky) glow, and may absorb the path.
        // Volumes: track each independently up to the surface; the earliest real collision
        // along the ray is the combined medium's first collision (independent exponential
        // processes), and only glow before it counts.
        let t_max = hit.as_ref().map_or(f64::INFINITY, |(h, _)| h.time);
        let tracked: Vec<_> = self.volumes.iter().map(|v| v.track(&ray, 0.0, t_max, rng)).collect();
        let first = tracked
            .iter()
            .enumerate()
            .min_by(|a, b| a.1 .1.t().total_cmp(&b.1 .1.t()))
            .filter(|(_, (_, e))| e.t().is_finite());
        let t_first = first.map_or(f64::INFINITY, |(_, (_, e))| e.t());
        let glow: Color = tracked
            .iter()
            .flat_map(|(g, _)| g.iter())
            .filter(|(t, _)| *t <= t_first)
            .map(|(_, c)| *c)
            .sum();
        if let Some((i, (_, event))) = first {
            return match *event {
                Event::Scattered { t, weight } => {
                    glow + weight.component_mul(&self.scatter(&self.volumes[i], &ray, t, bounces, rng))
                }
                _ => glow, // absorbed
            };
        }
        let Some((h, object)) = hit else {
            return glow + self.scene.environment.get_color(&ray.dir);
        };
        let pos = ray.at(h.time);
        let material = object.material;
        let wo = -glm::normalize(&ray.dir);

        let mut color = glow + material.emittance * material.color;
        color += self.sample_lights(&material, &pos, &h.normal, &wo, rng);
        if bounces < self.max_bounces {
            if let Some((wi, pdf)) = material.sample_f(&h.normal, &wo, rng) {
                let f = material.bsdf(&h.normal, &wo, &wi);
                let next = Ray { origin: pos, dir: wi };
                let indirect = 1.0 / pdf
                    * f.component_mul(&self.trace(next, bounces + 1, rng))
                    * wi.dot(&h.normal).abs();
                color += indirect.map(|c| c.min(FIREFLY_CLAMP));
            }
        }
        color
    }

    /// Light leaving a scattering point inside a volume toward the ray's origin: direct light
    /// from every light (through surfaces and volumes), weighted by the phase function, plus
    /// one phase-sampled indirect path.
    fn scatter(&self, volume: &Volume, ray: &Ray, t: f64, bounces: u32, rng: &mut StdRng) -> Color {
        let pos = ray.at(t);
        let forward = glm::normalize(&ray.dir);
        let mut color = glm::vec3(0.0, 0.0, 0.0);
        for light in &self.scene.lights {
            if let Light::Ambient(ambient) = light {
                color += *ambient;
                continue;
            }
            let (intensity, wi, dist) = light.illuminate(&pos, rng);
            let shadow = Ray { origin: pos, dir: wi };
            if self.closest_hit(shadow).is_some_and(|(h, _)| h.time <= dist) {
                continue;
            }
            let through: f64 = self.volumes.iter().map(|v| v.transmittance(&shadow, 0.0, dist, rng)).product();
            color += (intensity * phase_hg(volume.anisotropy, forward.dot(&wi)) * through)
                .map(|c| c.min(DIRECT_SCATTER_CLAMP));
        }
        if bounces < self.max_bounces {
            let next = Ray { origin: pos, dir: sample_hg(volume.anisotropy, &forward, rng) };
            color += self.trace(next, bounces + 1, rng).map(|c| c.min(FIREFLY_CLAMP));
        }
        color
    }

    /// Direct lighting: sample every light, checking visibility with a shadow ray.
    fn sample_lights(&self, material: &Material, pos: &glm::DVec3, n: &glm::DVec3, wo: &glm::DVec3, rng: &mut StdRng) -> Color {
        let mut color = glm::vec3(0.0, 0.0, 0.0);
        for light in &self.scene.lights {
            if let Light::Ambient(ambient) = light {
                color += ambient.component_mul(&material.color);
                continue;
            }
            let (intensity, wi, dist) = light.illuminate(pos, rng);
            let blocked = self
                .closest_hit(Ray { origin: *pos, dir: wi })
                .is_some_and(|(h, _)| h.time <= dist);
            if !blocked {
                // Light dims passing through volumes on the way.
                let shadow = Ray { origin: *pos, dir: wi };
                let through: f64 = self.volumes.iter().map(|v| v.transmittance(&shadow, 0.0, dist, rng)).product();
                if through > 0.0 {
                    color += material.bsdf(n, wo, &wi).component_mul(&intensity) * wi.dot(n) * through;
                }
            }
        }
        color
    }

    fn closest_hit(&self, ray: Ray) -> Option<(HitRecord, &Object)> {
        let mut h = HitRecord::new();
        let mut hit = None;
        for object in &self.scene.objects {
            if object.shape.intersect(&ray, EPSILON, &mut h) {
                hit = Some(object);
            }
        }
        Some((h, hit?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{VolumeSpec, Volume};
    use fidget::context::Tree;
    use rpt::Environment;

    /// Furnace test: white, non-absorbing smoke under a uniform sky (no surfaces, no lights)
    /// must look exactly as bright as the sky, however dense it is.
    #[test]
    fn white_smoke_in_a_furnace_matches_the_sky() {
        let sky = 0.6;
        let mut scene = Scene::new();
        scene.environment = Environment::Color(glm::vec3(sky, sky, sky));
        let (x, y, z) = (Tree::x(), Tree::y(), Tree::z());
        let r = (x.square() + y.square() + (z - 2.0).square()).sqrt();
        let density = ((1.0 - r) * 1000.0).min(Tree::constant(3.0));
        let smoke = Volume::new(VolumeSpec {
            density,
            temperature: None,
            albedo: Some(glm::vec3(1.0, 1.0, 1.0)),
            anisotropy: 0.3,
            brightness: 1.0,
        })
        .unwrap();
        let tracer = Tracer { scene: &scene, volumes: std::slice::from_ref(&smoke), max_bounces: 256 };
        let mut rng = StdRng::seed_from_u64(4);
        let n = 4000;
        let ray = Ray { origin: glm::vec3(0.0, -5.0, 2.0), dir: glm::vec3(0.0, 1.0, 0.0) };
        let mean = (0..n).map(|_| tracer.trace(ray, 0, &mut rng).x).sum::<f64>() / n as f64;
        assert!((mean - sky).abs() < 0.03, "mean radiance {mean:.4}, sky {sky}");
    }
}
