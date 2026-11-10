//! Renders an arbitrary rectangle of the image.
//!
//! rpt's `Renderer` only renders whole frames, and its per-pixel functions are private, so
//! this ports its integrator (`get_color` / `trace_ray` / `sample_lights` from
//! rpt 0.2.1's renderer.rs, MIT, (c) Eric Zhang and Alexander Morozov) on top of rpt's
//! public scene, camera, material and light types. Pixel coordinates match rpt's, so tiles
//! stitch into exactly the frame `Renderer::render` would frame.
//! rpt's license is reproduced in THIRD_PARTY_NOTICES.md.

use rand::{rngs::StdRng, Rng, SeedableRng};
use rpt::{color_bytes, glm, Color, HitRecord, Light, Material, Ray, Scene, Shape};

use crate::{
    shader::Shader,
    volume::{phase_hg, sample_hg, Event, Volume},
    RenderParams, Tile,
};

const EPSILON: f64 = 1e-12;
const FIREFLY_CLAMP: f64 = 100.0;
/// Cap on one light's direct contribution at a scattering point in smoke. A point light's
/// intensity grows as 1/distance², so scattering points sampled right next to one would
/// otherwise show up as isolated bright specks ("fireflies") all over the smoke.
const DIRECT_SCATTER_CLAMP: f64 = 8.0;

/// Adaptive sampling: `RenderParams::samples` is each pixel's maximum. A pixel takes
/// `min_samples` paths, then more in batches of `BATCH` until its estimate is within
/// `NOISE_TOLERANCE` of converged, measured on screen (after gamma) so dark pixels, where the
/// eye sees noise most, get the most samples. Flat, smooth regions stop early; edges, soft
/// shadows, glass and smoke keep going.
const BATCH: u32 = 8;
/// Target standard error of a pixel channel in display units (0..1): one and a half 8-bit
/// steps. Halving it costs about 4x the samples; at 1.5 steps a 256-sample cap traces about
/// half its paths and a 1024 cap a quarter, on a test scene with glass and soft shadows.
const NOISE_TOLERANCE: f64 = 1.5 / 255.0;
/// Below this linear value, the display error is measured as if the pixel were this bright:
/// the gamma curve is infinitely steep at black, and a pixel this dark shows as 0..30 anyway.
const DARK_FLOOR: f64 = 0.005;

/// Every pixel's minimum: enough paths that a rarely hit light or caustic is unlikely to be
/// missed entirely (which would read as a converged pixel), and at most the maximum.
fn min_samples(max: u32) -> u32 {
    max.min(16.max(max / 16))
}

/// Render `tile` as packed sRGB bytes (row-major, `w * h * 3`); `Err` if the shape script is invalid.
pub fn render_tile(params: &RenderParams, tile: &Tile) -> Result<Vec<u8>, String> {
    render_tile_counted(params, tile).map(|(rgb, _)| rgb)
}

/// [`render_tile`], plus the number of paths it traced.
pub(crate) fn render_tile_counted(params: &RenderParams, tile: &Tile) -> Result<(Vec<u8>, u64), String> {
    let built = crate::build_scene(params)?;
    let camera = params.camera.camera();
    let tracer = Tracer {
        scene: &built.scene,
        volumes: &built.volumes,
        shaders: &built.shaders,
        sky: built.sky.as_ref(),
        void_floor: built.void_floor,
        hidden_sky: built.hidden_sky,
        max_bounces: params.bounces,
    };
    // Seeded from the tile position, so a retried tile renders identically.
    let mut rng = StdRng::seed_from_u64((u64::from(tile.y) << 32) | u64::from(tile.x));

    let (width, height) = (params.width, params.height);
    let dim = width.max(height) as f64;
    let first = min_samples(params.samples);
    let mut traced = 0u64;
    let mut out = Vec::with_capacity((tile.w * tile.h * 3) as usize);
    for y in tile.y..tile.y + tile.h {
        for x in tile.x..tile.x + tile.w {
            // Normalized to rpt's [-1, 1] box over the longer dimension.
            let xn = ((2 * x + 1) as f64 - width as f64) / dim;
            let yn = ((2 * (height - y) - 1) as f64 - height as f64) / dim;
            let mut pixel = Pixel::default();
            let mut target = first;
            loop {
                while pixel.n < target {
                    let dx = rng.gen_range((-1.0 / dim)..(1.0 / dim));
                    let dy = rng.gen_range((-1.0 / dim)..(1.0 / dim));
                    pixel.add(tracer.trace(camera.cast_ray(xn + dx, yn + dy, &mut rng), 0, true, &mut rng));
                }
                if target >= params.samples || pixel.converged() {
                    break;
                }
                target = (target + BATCH).min(params.samples);
            }
            traced += u64::from(pixel.n);
            out.extend(color_bytes(&pixel.mean()));
        }
    }
    Ok((out, traced))
}

/// One pixel's running sums, per channel, for its mean and variance.
#[derive(Default)]
struct Pixel {
    n: u32,
    sum: [f64; 3],
    sum_sq: [f64; 3],
}

impl Pixel {
    fn add(&mut self, c: Color) {
        self.n += 1;
        for (i, v) in [c.x, c.y, c.z].into_iter().enumerate() {
            self.sum[i] += v;
            self.sum_sq[i] += v * v;
        }
    }

    fn mean(&self) -> Color {
        let n = f64::from(self.n);
        glm::vec3(self.sum[0] / n, self.sum[1] / n, self.sum[2] / n)
    }

    /// Whether every channel's standard error, carried through the display gamma, is within
    /// `NOISE_TOLERANCE`. Channels that are surely brighter than white clip, so they're done.
    fn converged(&self) -> bool {
        let n = f64::from(self.n);
        (0..3).all(|i| {
            let mean = self.sum[i] / n;
            let var = ((self.sum_sq[i] / n - mean * mean) * n / (n - 1.0)).max(0.0);
            let err = (var / n).sqrt();
            if mean - 2.0 * err > 1.0 {
                return true;
            }
            // d/dv v^(1/2.2): how much the displayed value moves per unit of linear value.
            let slope = mean.clamp(DARK_FLOOR, 1.0).powf(1.0 / 2.2 - 1.0) / 2.2;
            err * slope <= NOISE_TOLERANCE
        })
    }
}

/// What one light contributes at a point.
enum Incoming {
    /// Ambient light: the same from every direction, never shadowed.
    Ambient(Color),
    /// Radiance arriving from direction `wi` (toward the light).
    Direct(Color, glm::DVec3),
    Blocked,
}

struct Tracer<'a> {
    scene: &'a Scene,
    /// Glowing, absorbing gas (fire); see `volume.rs`.
    volumes: &'a [Volume],
    /// Per object in `scene.objects`, the shader that sets its material at each hit.
    shaders: &'a [Option<Shader>],
    /// The sky's shader, replacing `scene.environment`.
    sky: Option<&'a Shader>,
    /// Object 0 (the floor) absorbs all light: the black void.
    void_floor: bool,
    /// The sky lights the scene but shows black to the camera.
    hidden_sky: bool,
    max_bounces: u32,
}

impl Tracer<'_> {
    /// Monte Carlo estimate of the radiance along `ray`. `seen`: the ray carries what the camera
    /// sees directly (a camera ray, or one continuing through or off glass), where a hidden sky
    /// shows black.
    fn trace(&self, ray: Ray, bounces: u32, seen: bool, rng: &mut StdRng) -> Color {
        let hit = self.closest_hit(ray);
        // Volumes in front of the surface (or the sky) glow, and may absorb or scatter the path.
        // Each is tracked independently up to the surface; the earliest real collision along the
        // ray is the combined medium's first collision (independent exponential processes), and
        // only glow before it counts.
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
        let Some((h, index)) = hit else {
            if seen && self.hidden_sky {
                return glow;
            }
            let sky = self.sky.and_then(|s| s.sky(&ray.dir));
            return glow + sky.unwrap_or_else(|| self.scene.environment.get_color(&ray.dir));
        };
        if index == 0 && self.void_floor {
            return glow;
        }
        let pos = ray.at(h.time);
        let mut material = self.scene.objects[index].material;
        let wo = -glm::normalize(&ray.dir);
        // The shading normal: the surface's, unless a shader's bump tilts it.
        let mut normal = h.normal;
        if let Some(Some(shader)) = self.shaders.get(index) {
            shader.shade(&mut material, &pos, &h.normal);
            normal = shader.bumped_normal(&pos, &h.normal);
            // A bump tilted away from the viewer (at grazing angles) would shade an opaque
            // surface black from the front; keep the true normal there.
            if !material.transparent && wo.dot(&normal) <= 0.0 && wo.dot(&h.normal) > 0.0 {
                normal = h.normal;
            }
        }

        let mut color = glow + material.emittance * material.color;
        color += self.sample_lights(&material, &pos, &normal, &wo, rng);
        if bounces < self.max_bounces {
            if let Some((wi, pdf)) = material.sample_f(&normal, &wo, rng) {
                // An opaque surface can't send light into itself, whatever the bump says.
                let into_surface = !material.transparent && wi.dot(&h.normal) * wo.dot(&h.normal) < 0.0;
                if !into_surface {
                    let f = material.bsdf(&normal, &wo, &wi);
                    let next = Ray { origin: pos, dir: wi };
                    let indirect = 1.0 / pdf
                        * f.component_mul(&self.trace(next, bounces + 1, seen && material.transparent, rng))
                        * wi.dot(&normal).abs();
                    color += indirect.map(|c| c.min(FIREFLY_CLAMP));
                }
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
            match self.light_at(light, &pos, rng) {
                Incoming::Ambient(ambient) => color += ambient,
                Incoming::Direct(intensity, wi) => {
                    color += (intensity * phase_hg(volume.anisotropy, forward.dot(&wi))).map(|c| c.min(DIRECT_SCATTER_CLAMP))
                }
                Incoming::Blocked => {}
            }
        }
        if bounces < self.max_bounces {
            let next = Ray { origin: pos, dir: sample_hg(volume.anisotropy, &forward, rng) };
            color += self.trace(next, bounces + 1, false, rng).map(|c| c.min(FIREFLY_CLAMP));
        }
        color
    }

    /// Direct lighting: sample every light, checking visibility with a shadow ray.
    fn sample_lights(&self, material: &Material, pos: &glm::DVec3, n: &glm::DVec3, wo: &glm::DVec3, rng: &mut StdRng) -> Color {
        let mut color = glm::vec3(0.0, 0.0, 0.0);
        for light in &self.scene.lights {
            match self.light_at(light, pos, rng) {
                Incoming::Ambient(ambient) => color += ambient.component_mul(&material.color),
                Incoming::Direct(intensity, wi) => color += material.bsdf(n, wo, &wi).component_mul(&intensity) * wi.dot(n),
                Incoming::Blocked => {}
            }
        }
        color
    }

    /// Light from `light` arriving at `pos`: a shadow ray checks surfaces, and ratio tracking
    /// dims it by the volumes on the way.
    fn light_at(&self, light: &Light, pos: &glm::DVec3, rng: &mut StdRng) -> Incoming {
        if let Light::Ambient(ambient) = light {
            return Incoming::Ambient(*ambient);
        }
        let (intensity, wi, dist) = light.illuminate(pos, rng);
        let shadow = Ray { origin: *pos, dir: wi };
        if self.closest_hit(shadow).is_some_and(|(h, _)| h.time <= dist) {
            return Incoming::Blocked;
        }
        let through: f64 = self.volumes.iter().map(|v| v.transmittance(&shadow, 0.0, dist, rng)).product();
        if through > 0.0 {
            Incoming::Direct(intensity * through, wi)
        } else {
            Incoming::Blocked
        }
    }

    /// The nearest hit along `ray`, and the index of the object it hit in `scene.objects`.
    fn closest_hit(&self, ray: Ray) -> Option<(HitRecord, usize)> {
        let mut h = HitRecord::new();
        let mut hit = None;
        for (i, object) in self.scene.objects.iter().enumerate() {
            if object.shape.intersect(&ray, EPSILON, &mut h) {
                hit = Some(i);
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
        let tracer = Tracer { scene: &scene, volumes: std::slice::from_ref(&smoke), shaders: &[], sky: None, void_floor: false, hidden_sky: false, max_bounces: 256 };
        let mut rng = StdRng::seed_from_u64(4);
        let n = 4000;
        let ray = Ray { origin: glm::vec3(0.0, -5.0, 2.0), dir: glm::vec3(0.0, 1.0, 0.0) };
        let mean = (0..n).map(|_| tracer.trace(ray, 0, true, &mut rng).x).sum::<f64>() / n as f64;
        assert!((mean - sky).abs() < 0.03, "mean radiance {mean:.4}, sky {sky}");
    }
}
