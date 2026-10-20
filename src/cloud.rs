//! Paints a soft cumulus cloud around the text area, in three layers that the UI
//! animates separately: the body, a fringe of small puffs on its silhouette, and
//! faint wisps trailing off its sides and base.
//!
//! The puffs are summed into one smooth density field (like metaballs) with
//! fractal noise roughening the edges. That density, plus fine billows, is lit
//! as a height field from the upper left, so the cloud has soft highlights,
//! blue-grey shade underneath and translucent, glowing edges. The text area is
//! kept bright and opaque so text stays readable.

use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Everything in logical pixels, relative to the window.
pub struct Spec {
    pub width: u32,
    pub height: u32,
    /// The text area: left, top, right, bottom.
    pub core: [f32; 4],
    /// Seed for the cloud's shape; the same seed gives the same cloud.
    pub seed: u32,
}

pub struct Layers {
    pub body: SharedPixelBuffer<Rgba8Pixel>,
    pub fringe: SharedPixelBuffer<Rgba8Pixel>,
    pub wisps: SharedPixelBuffer<Rgba8Pixel>,
    /// Where the cloud can be seen, widened by how far the UI moves the layers:
    /// rectangles `[left, top, right, bottom]` for the window's click region.
    pub region: Vec<[i32; 4]>,
}

/// How far (logical px) the UI moves the layers (bob, lift, drift).
const MOTION_X: usize = 10;
const MOTION_Y: usize = 12;
/// Below this alpha (out of 255) the cloud counts as empty sky.
const VISIBLE_ALPHA: u8 = 13;

#[derive(Clone, Copy)]
struct Puff {
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
}

struct Style {
    opacity: f32,
    edge_noise: f32,
    edge_softness: f32,
    /// Noise frequency stretch along x (wisps are streaky).
    stretch: f32,
    /// The body layer: a solid plateau under the text area, kept bright and
    /// opaque so text stays readable.
    solid_core: bool,
    /// Brightness of faces turned away from the light.
    base_light: f32,
    /// How solid each puff is; higher means a firmer outline.
    puff_density: f32,
}

pub fn render(spec: &Spec) -> Layers {
    let mut rng = Rng(spec.seed.wrapping_mul(2654435761).max(1));
    let noise = Noise::new(spec.seed);
    let [x0, y0, x1, y1] = spec.core;
    let cw = x1 - x0;
    // Smaller text areas (and the minimized puff) get proportionally smaller billows.
    let k = (cw / 380.0).clamp(0.25, 1.0);

    // Body: big overlapping billows ring the text area, towering in a dome over
    // its top, rounder on the sides, flattened along the base.
    let mut body = Vec::new();
    let n_top = ((cw / 60.0) as usize).max(4);
    let mut top_puffs = Vec::new();
    for i in 0..n_top {
        let t = (i as f32 + 0.5) / n_top as f32;
        let dome = (-((t - 0.42) / 0.33).powi(2)).exp();
        // Irregular sizes and heights, so the top doesn't read as a row of bumps.
        let r = (30.0 + 42.0 * dome + rng.range(0.0, 18.0)) * k;
        let p = Puff {
            cx: x0 + 6.0 + t * (cw - 12.0) + rng.range(-14.0, 14.0),
            cy: y0 + r * rng.range(0.3, 0.5),
            rx: r * rng.range(1.0, 1.3),
            ry: r,
        };
        top_puffs.push(p);
        body.push(p);
        // Smaller billows growing out of its upper rim.
        for _ in 0..2 {
            let a = rng.range(-2.5, -0.65);
            let rs = r * rng.range(0.42, 0.55);
            body.push(Puff { cx: p.cx + a.cos() * r * 0.62, cy: p.cy + a.sin() * r * 0.62, rx: rs * 1.1, ry: rs });
        }
    }
    let n_side = (((y1 - y0) / 40.0) as usize + 1).max(3);
    for right in [false, true] {
        for i in 0..n_side {
            let t = i as f32 / (n_side - 1).max(1) as f32;
            // Bulging most at mid-height, so the ends are round.
            let bulge = (std::f32::consts::PI * t).sin();
            let r = (rng.range(36.0, 44.0) + 10.0 * bulge) * k;
            let inset = r * (0.35 - 0.3 * bulge);
            let cx = if right { x1 - inset } else { x0 + inset };
            body.push(Puff { cx, cy: y0 + 8.0 + t * (y1 - y0 - 8.0), rx: r, ry: r });
        }
    }
    let n_base = ((cw / 56.0) as usize).max(3);
    for i in 0..n_base {
        let t = (i as f32 + 0.5) / n_base as f32;
        let ry = rng.range(26.0, 32.0) * k;
        body.push(Puff {
            cx: x0 + t * cw + rng.range(-6.0, 6.0),
            cy: y1 - ry * 0.15,
            rx: rng.range(58.0, 76.0) * k,
            ry,
        });
    }

    // Fringe: soft small billows on the upper and side silhouette.
    let mut fringe = Vec::new();
    for p in &top_puffs {
        for _ in 0..2 {
            let a = rng.range(-2.7, -0.45);
            let rs = rng.range(14.0, 24.0) * k;
            let d = p.ry * rng.range(0.62, 0.8);
            fringe.push(Puff { cx: p.cx + a.cos() * d, cy: p.cy + a.sin() * d, rx: rs * 1.1, ry: rs });
        }
    }
    for i in 0..n_side * 2 {
        let right = i % 2 == 1;
        let rs = rng.range(14.0, 22.0) * k;
        let cy = rng.range(y0, y1 + 6.0);
        let cx = if right { x1 + rng.range(4.0, 12.0) * k } else { x0 - rng.range(4.0, 12.0) * k };
        fringe.push(Puff { cx, cy, rx: rs, ry: rs });
    }

    // Wisps: faint, torn streaks trailing off the sides and under the base.
    let mut wisps = Vec::new();
    for right in [false, true] {
        let rx = rng.range(34.0, 46.0) * k;
        let cx = if right { x1 + 12.0 * k } else { x0 - 12.0 * k };
        wisps.push(Puff { cx, cy: rng.range(y0, y1 + 4.0), rx, ry: rng.range(9.0, 13.0) * k });
    }

    let body_style = Style {
        opacity: 1.0,
        edge_noise: 0.6,
        edge_softness: 0.3,
        stretch: 1.0,
        solid_core: true,
        base_light: 0.45,
        puff_density: 1.45,
    };
    let fringe_style = Style {
        opacity: 0.8,
        edge_noise: 0.7,
        edge_softness: 0.55,
        stretch: 1.0,
        solid_core: false,
        base_light: 0.66,
        puff_density: 1.3,
    };
    let wisp_style = Style {
        opacity: 0.3,
        edge_noise: 1.4,
        edge_softness: 0.8,
        stretch: 0.35,
        solid_core: false,
        base_light: 0.85,
        puff_density: 1.0,
    };
    let body = paint(spec, &noise, &body, &body_style);
    let fringe = paint(spec, &noise, &fringe, &fringe_style);
    let wisps = paint(spec, &noise, &wisps, &wisp_style);

    // Click region: wherever any layer is visible, widened by the layers' motion.
    let (w, h) = (spec.width as usize, spec.height as usize);
    let alpha = |layer: &SharedPixelBuffer<Rgba8Pixel>, i: usize| layer.as_slice()[i].a;
    let mut mask: Vec<bool> =
        (0..w * h).map(|i| alpha(&body, i).max(alpha(&fringe, i)).max(alpha(&wisps, i)) >= VISIBLE_ALPHA).collect();
    dilate(&mut mask, w, h, MOTION_X, MOTION_Y);
    let region = mask_to_rects(&mask, w);
    Layers { body, fringe, wisps, region }
}

/// The mask's runs of visible pixels, row by row, with identical rows next to
/// each other merged into one taller rectangle.
fn mask_to_rects(mask: &[bool], width: usize) -> Vec<[i32; 4]> {
    let mut rects: Vec<[i32; 4]> = Vec::new();
    let mut band_start = 0; // Index in `rects` where the previous row's runs begin.
    let mut previous_runs: Vec<(i32, i32)> = Vec::new();
    for (y, row) in mask.chunks(width).enumerate() {
        let y = y as i32;
        let mut runs = Vec::new();
        let mut x = 0;
        while x < row.len() {
            if row[x] {
                let start = x;
                while x < row.len() && row[x] {
                    x += 1;
                }
                runs.push((start as i32, x as i32));
            } else {
                x += 1;
            }
        }
        if !runs.is_empty() && runs == previous_runs {
            for rect in &mut rects[band_start..] {
                rect[3] = y + 1;
            }
        } else {
            band_start = rects.len();
            rects.extend(runs.iter().map(|&(x0, x1)| [x0, y, x1, y + 1]));
        }
        previous_runs = runs;
    }
    rects
}

fn paint(spec: &Spec, noise: &Noise, puffs: &[Puff], style: &Style) -> SharedPixelBuffer<Rgba8Pixel> {
    let (w, h) = (spec.width as usize, spec.height as usize);
    let [x0, y0, x1, y1] = spec.core;

    // 1. Density: puffs blend smoothly into one mass (metaball-style sum),
    //    the text area is a solid plateau, and noise roughens the edges.
    let mut density = vec![0.0f32; w * h];
    par_rows(&mut density, w, |py, row| {
        for (px, out) in row.iter_mut().enumerate() {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let mut d = 0.0;
            for p in puffs {
                let (ux, uy) = ((x - p.cx) / p.rx, (y - p.cy) / p.ry);
                let q = ux * ux + uy * uy;
                if q < 1.0 {
                    d += (1.0 - q) * (1.0 - q) * style.puff_density;
                }
            }
            if style.solid_core {
                // Plateau ends inside the ring of billows, which shape the outline.
                d += 1.2 * smoothstep(4.0, 40.0, round_rect_inside(x, y, x0, y0, x1, y1, 30.0));
            }
            let rough = noise.fbm(x / 30.0 * style.stretch, y / 30.0) * 0.82
                + noise.fbm(x / 10.0 * style.stretch + 17.0, y / 10.0) * 0.18;
            // Only the edge band is roughened: empty sky stays empty, the inside keeps its shape.
            d += rough * style.edge_noise * smoothstep(0.0, 0.3, d) * (1.0 - smoothstep(0.35, 1.3, d));
            *out = d;
        }
    });

    // 2. Treat the density as a height field with fine billows and light it
    //    from the upper left; faces turned down fall into soft blue-grey shade.
    let mut height = vec![0.0f32; w * h];
    par_rows(&mut height, w, |py, row| {
        for (px, out) in row.iter_mut().enumerate() {
            let (x, y) = (px as f32, py as f32);
            *out = density[py * w + px].max(0.0).sqrt() * 26.0
                + noise.fbm(x / 22.0 + 51.0, y / 22.0) * 3.0
                + noise.fbm(x / 9.0 + 7.0, y / 9.0) * 0.7;
        }
    });
    let light = normalize([-0.45, -0.8, 0.75]);
    let shadow = [0.60, 0.67, 0.79];
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
    par_rows(buffer.make_mut_slice(), w, |py, row| {
        for (px, out) in row.iter_mut().enumerate() {
            let i = py * w + px;
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let d = density[i];
            let mut alpha = smoothstep(0.06, 0.06 + style.edge_softness, d);
            let thin = 1.0 - alpha;

            let hx = height[py * w + (px + 1).min(w - 1)] - height[py * w + px.saturating_sub(1)];
            let hy = height[(py + 1).min(h - 1) * w + px] - height[py.saturating_sub(1) * w + px];
            let n = normalize([-hx * 0.5, -hy * 0.5, 1.0]);
            let diffuse = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]).max(0.0);
            let mut b = style.base_light + (1.04 - style.base_light) * diffuse;
            // The flat base sits in its own shade.
            b -= 0.12 * smoothstep(y1 - 24.0, y1 + 34.0, y);
            // Thin vapour at the edges glows with light passing through.
            b += (1.0 - b) * thin.powf(1.2) * 0.6;

            if style.solid_core {
                // Solid and bright under the text (a little beyond it too), while
                // the billows still show faintly.
                let text = smoothstep(-24.0, 8.0, round_rect_inside(x, y, x0, y0, x1, y1, 26.0));
                b += (0.99 - b) * text * 0.9;
                alpha = alpha.max(text);
            }

            // No hard cut at the window edge.
            let border = (x.min(y).min(w as f32 - x).min(h as f32 - y) / 10.0).clamp(0.0, 1.0);
            alpha *= style.opacity * border;

            let b = b.clamp(0.0, 1.0);
            let c = |k: usize| ((shadow[k] + (1.0 - shadow[k]) * b) * 255.0) as u8;
            *out = Rgba8Pixel { r: c(0), g: c(1), b: c(2), a: (alpha * 255.0) as u8 };
        }
    });
    buffer
}

/// Runs `f(row_index, row)` over the rows of a `width`-wide image, split into
/// blocks across all CPU cores.
fn par_rows<T: Send>(data: &mut [T], width: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let rows = data.len() / width.max(1);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, rows.max(1));
    let per = rows.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (block_index, block) in data.chunks_mut(per * width).enumerate() {
            let f = &f;
            scope.spawn(move || {
                for (j, row) in block.chunks_mut(width).enumerate() {
                    f(block_index * per + j, row);
                }
            });
        }
    });
}

/// Distance inside a rounded rectangle (positive inside, negative outside).
fn round_rect_inside(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (hx, hy) = ((x1 - x0) / 2.0 - r, (y1 - y0) / 2.0 - r);
    let (qx, qy) = ((x - cx).abs() - hx, (y - cy).abs() - hy);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0);
    r - outside
}

/// Grows the mask by rx/ry pixels in each direction.
fn dilate(mask: &mut [bool], w: usize, h: usize, rx: usize, ry: usize) {
    let mut tmp = vec![false; mask.len()];
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (x.saturating_sub(rx), (x + rx).min(w - 1));
            tmp[y * w + x] = mask[y * w + a..=y * w + b].iter().any(|&v| v);
        }
    }
    for y in 0..h {
        let (a, b) = (y.saturating_sub(ry), (y + ry).min(h - 1));
        for x in 0..w {
            mask[y * w + x] = (a..=b).any(|yy| tmp[yy * w + x]);
        }
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / l, v[1] / l, v[2] / l]
}

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

/// 2D Perlin noise with fractal sums, roughly in [-1, 1].
struct Noise {
    perm: [u8; 512],
}

impl Noise {
    fn new(seed: u32) -> Self {
        let mut p: Vec<u8> = (0..=255).collect();
        let mut rng = Rng(seed.wrapping_mul(747796405).max(1));
        for i in (1..256).rev() {
            let j = (rng.next() * (i + 1) as f32) as usize % (i + 1);
            p.swap(i, j);
        }
        let mut perm = [0u8; 512];
        for i in 0..512 {
            perm[i] = p[i & 255];
        }
        Noise { perm }
    }

    fn grad(&self, ix: i32, iy: i32, dx: f32, dy: f32) -> f32 {
        let h = self.perm[(self.perm[(ix & 255) as usize] as usize + (iy & 255) as usize) & 511] & 7;
        let (gx, gy) =
            [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (0.7, 0.7), (-0.7, 0.7), (0.7, -0.7), (-0.7, -0.7)]
                [h as usize];
        gx * dx + gy * dy
    }

    fn noise(&self, x: f32, y: f32) -> f32 {
        let (ix, iy) = (x.floor() as i32, y.floor() as i32);
        let (fx, fy) = (x - x.floor(), y - y.floor());
        let fade = |t: f32| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
        let (u, v) = (fade(fx), fade(fy));
        let a = self.grad(ix, iy, fx, fy);
        let b = self.grad(ix + 1, iy, fx - 1.0, fy);
        let c = self.grad(ix, iy + 1, fx, fy - 1.0);
        let d = self.grad(ix + 1, iy + 1, fx - 1.0, fy - 1.0);
        let top = a + u * (b - a);
        let bottom = c + u * (d - c);
        (top + v * (bottom - top)) * 1.4
    }

    fn fbm(&self, x: f32, y: f32) -> f32 {
        let (mut sum, mut amp, mut freq) = (0.0, 0.5, 1.0);
        for _ in 0..4 {
            sum += amp * self.noise(x * freq, y * freq);
            amp *= 0.5;
            freq *= 2.0;
        }
        sum * 1.1
    }
}

#[cfg(test)]
mod tests {
    /// Writes the layers as raw RGBA for inspection:
    /// cargo test --release dump_cloud -- --ignored  (then view cloud-*.rgba, see the skill)
    #[test]
    #[ignore]
    fn dump_cloud() {
        let dir = std::env::var("CLOUD_DUMP_DIR").unwrap_or_else(|_| ".".into());
        let seed = std::env::var("CLOUD_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(7);
        let spec = super::Spec { width: 484, height: 200, core: [52.0, 70.0, 432.0, 148.0], seed };
        let t = std::time::Instant::now();
        let layers = super::render(&spec);
        println!("rendered in {} ms", t.elapsed().as_millis());
        for (name, buf) in [("body", &layers.body), ("fringe", &layers.fringe), ("wisps", &layers.wisps)] {
            std::fs::write(format!("{dir}/cloud-{name}.rgba"), buf.as_bytes()).unwrap();
        }
    }

    #[test]
    fn identical_rows_merge_into_one_rectangle() {
        let mask: Vec<bool> = [".##.", ".##.", "#..#"].iter().flat_map(|row| row.chars().map(|c| c == '#')).collect();
        assert_eq!(super::mask_to_rects(&mask, 4), vec![[1, 0, 3, 2], [0, 2, 1, 3], [3, 2, 4, 3]]);
    }
}
