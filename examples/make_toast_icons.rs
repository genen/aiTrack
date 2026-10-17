// SPDX-License-Identifier: GPL-3.0-or-later
//! Generates the Windows toast logos: the Filled badge of each vendor drawn by the same painters
//! as the window (so it always matches), rendered at 4x with a software rasteriser, with a
//! transparent background (two renders over black and white give the alpha) and box-filtered down:
//!
//!   cargo run --example make_toast_icons -- assets/toast
//!
//! writes `<vendor>-96.png` and `<vendor>-192.png`; the 192 px files are embedded by `src/toast.rs`.

use aitrack::settings::BadgeVariant;
use aitrack::theme::DARK;
use aitrack::ui::paint_vendor_badge;
use aitrack::vendor::VendorId;
use eframe::egui::{self, epaint, Color32, Context, Pos2, RawInput, Rect, TextureId};
use std::collections::HashMap;
use std::path::PathBuf;

struct Tex {
    w: usize,
    h: usize,
    px: Vec<Color32>,
}

fn rasterize(
    prims: &[egui::ClippedPrimitive],
    texs: &HashMap<TextureId, Tex>,
    bg: Color32,
    ppp: f32,
    side: f32,
) -> (usize, Vec<[f32; 3]>) {
    let (pw, ph) = ((side * ppp) as usize, (side * ppp) as usize);
    let mut buf = vec![
        [
            bg.r() as f32 / 255.0,
            bg.g() as f32 / 255.0,
            bg.b() as f32 / 255.0
        ];
        pw * ph
    ];
    for prim in prims {
        let epaint::Primitive::Mesh(mesh) = &prim.primitive else {
            continue;
        };
        let Some(tex) = texs.get(&mesh.texture_id) else {
            continue;
        };
        let clip = prim.clip_rect;
        let (cx0, cy0) = (
            (clip.min.x * ppp).max(0.0) as usize,
            (clip.min.y * ppp).max(0.0) as usize,
        );
        let (cx1, cy1) = (
            ((clip.max.x * ppp).ceil() as usize).min(pw),
            ((clip.max.y * ppp).ceil() as usize).min(ph),
        );
        for tri in mesh.indices.chunks(3) {
            let v: Vec<_> = tri.iter().map(|&i| mesh.vertices[i as usize]).collect();
            let p: Vec<(f32, f32)> = v.iter().map(|v| (v.pos.x * ppp, v.pos.y * ppp)).collect();
            let area =
                (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
            if area.abs() < 1e-6 {
                continue;
            }
            let minx = p
                .iter()
                .map(|q| q.0)
                .fold(f32::MAX, f32::min)
                .floor()
                .max(cx0 as f32) as usize;
            let maxx = (p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as usize).min(cx1);
            let miny = p
                .iter()
                .map(|q| q.1)
                .fold(f32::MAX, f32::min)
                .floor()
                .max(cy0 as f32) as usize;
            let maxy = (p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as usize).min(cy1);
            for y in miny..maxy {
                for x in minx..maxx {
                    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                    let w0 = ((p[1].0 - px) * (p[2].1 - py) - (p[2].0 - px) * (p[1].1 - py)) / area;
                    let w1 = ((p[2].0 - px) * (p[0].1 - py) - (p[0].0 - px) * (p[2].1 - py)) / area;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    let u = w0 * v[0].uv.x + w1 * v[1].uv.x + w2 * v[2].uv.x;
                    let vv = w0 * v[0].uv.y + w1 * v[1].uv.y + w2 * v[2].uv.y;
                    let (tx, ty) = (
                        ((u * tex.w as f32) as usize).min(tex.w - 1),
                        ((vv * tex.h as f32) as usize).min(tex.h - 1),
                    );
                    let t = tex.px[ty * tex.w + tx].to_array().map(|c| c as f32 / 255.0);
                    let col: Vec<f32> = (0..4)
                        .map(|k| {
                            let c = |i: usize| v[i].color.to_array()[k] as f32 / 255.0;
                            (w0 * c(0) + w1 * c(1) + w2 * c(2)) * t[k]
                        })
                        .collect();
                    let d = &mut buf[y * pw + x];
                    for k in 0..3 {
                        d[k] = col[k] + d[k] * (1.0 - col[3]);
                    }
                }
            }
        }
    }
    (pw, buf)
}

/// One render of the badge on `bg`: `side` points at `ppp` pixels per point.
fn render(v: VendorId, ppp: f32, side: f32, bg: Color32) -> (usize, Vec<[f32; 3]>) {
    let ctx = Context::default();
    ctx.set_pixels_per_point(ppp);
    let mut texs: HashMap<TextureId, Tex> = HashMap::new();
    let mut prims = Vec::new();
    for i in 0..3 {
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(side, side))),
            time: Some(i as f64 * 0.1),
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| {
            let p = ctx.layer_painter(egui::LayerId::background());
            let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(side, side));
            paint_vendor_badge(&p, rect, v, BadgeVariant::Filled, &DARK);
        });
        for (id, delta) in &out.textures_delta.set {
            let (w, h) = (delta.image.width(), delta.image.height());
            let pixels: Vec<Color32> = match &delta.image {
                epaint::ImageData::Color(c) => c.pixels.clone(),
                epaint::ImageData::Font(f) => f.srgba_pixels(None).collect(),
            };
            let tex = texs.entry(*id).or_insert_with(|| Tex {
                w: 0,
                h: 0,
                px: vec![],
            });
            match delta.pos {
                None => *tex = Tex { w, h, px: pixels },
                Some([x0, y0]) => {
                    for y in 0..h {
                        for x in 0..w {
                            tex.px[(y0 + y) * tex.w + x0 + x] = pixels[y * w + x];
                        }
                    }
                }
            }
        }
        prims = ctx.tessellate(out.shapes, ppp);
    }
    rasterize(&prims, &texs, bg, ppp, side)
}

/// RGBA of the badge with a real alpha channel, `px` x `px` pixels (4x supersampled).
fn icon(v: VendorId, px: usize) -> Vec<u8> {
    let side = 24.0_f32; // points; ppp = px * 4 / side
    let ppp = px as f32 * 4.0 / side;
    let (n, on_black) = render(v, ppp, side, Color32::BLACK);
    let (_, on_white) = render(v, ppp, side, Color32::WHITE);
    // premultiplied colour and alpha per high-res pixel
    let mut hi = vec![[0.0f32; 4]; n * n];
    for i in 0..n * n {
        let (b, w) = (on_black[i], on_white[i]);
        let alpha = (1.0 - ((w[0] - b[0]) + (w[1] - b[1]) + (w[2] - b[2])) / 3.0).clamp(0.0, 1.0);
        hi[i] = [b[0], b[1], b[2], alpha]; // over black: colour * alpha, already premultiplied
    }
    let f = n / px;
    let mut out = Vec::with_capacity(px * px * 4);
    for y in 0..px {
        for x in 0..px {
            let mut acc = [0.0f32; 4];
            for dy in 0..f {
                for dx in 0..f {
                    let h = hi[(y * f + dy) * n + x * f + dx];
                    for k in 0..4 {
                        acc[k] += h[k];
                    }
                }
            }
            let cnt = (f * f) as f32;
            let a = acc[3] / cnt;
            let un = |c: f32| {
                if a > 1e-4 {
                    (c / cnt / a).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };
            out.extend([
                (un(acc[0]) * 255.0 + 0.5) as u8,
                (un(acc[1]) * 255.0 + 0.5) as u8,
                (un(acc[2]) * 255.0 + 0.5) as u8,
                (a * 255.0 + 0.5) as u8,
            ]);
        }
    }
    out
}

fn main() {
    let dir: PathBuf = std::env::args()
        .nth(1)
        .map(Into::into)
        .unwrap_or_else(|| "assets/toast".into());
    std::fs::create_dir_all(&dir).unwrap();
    for (name, v) in [("claude", VendorId::Claude), ("grok", VendorId::Grok)] {
        for px in [96usize, 192] {
            let path = dir.join(format!("{name}-{px}.png"));
            let file = std::fs::File::create(&path).unwrap();
            let mut enc = png::Encoder::new(file, px as u32, px as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .unwrap()
                .write_image_data(&icon(v, px))
                .unwrap();
            println!("wrote {}", path.display());
        }
    }
}
