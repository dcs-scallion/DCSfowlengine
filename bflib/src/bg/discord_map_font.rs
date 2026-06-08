//! Roboto Condensed Latin + Cyrillic labels on map PNGs (`assets/fonts/`).

use ab_glyph::{Font, FontRef, Glyph, PxScale, ScaleFont, point};
use image::RgbaImage;

pub struct MapLabelFont {
    latin: FontRef<'static>,
    cyrillic: FontRef<'static>,
}

impl MapLabelFont {
    pub fn embedded() -> Self {
        let latin = FontRef::try_from_slice(include_bytes!(
            "../../../assets/fonts/RobotoCondensed-latin-400.ttf"
        ))
        .expect("RobotoCondensed-latin-400.ttf");
        let cyrillic = FontRef::try_from_slice(include_bytes!(
            "../../../assets/fonts/RobotoCondensed-cyrillic-400.ttf"
        ))
        .expect("RobotoCondensed-cyrillic-400.ttf");
        Self { latin, cyrillic }
    }

    fn font_for(&self, ch: char) -> &FontRef<'_> {
        if matches!(ch, '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}') {
            &self.cyrillic
        } else {
            &self.latin
        }
    }

    /// White label with a thin dark halo (readable on light Mapbox terrain).
    pub fn draw_white(&self, base: &mut RgbaImage, x: i32, y: i32, text: &str, font_px: f32) {
        if text.is_empty() || font_px < 4.0 {
            return;
        }
        const HALO: [(i32, i32); 4] = [(0, -1), (0, 1), (-1, 0), (1, 0)];
        for (dx, dy) in HALO {
            self.draw_color(base, x + dx, y + dy, text, font_px, [20, 20, 24, 255]);
        }
        self.draw_color(base, x, y, text, font_px, [255, 255, 255, 255]);
    }

    fn draw_color(
        &self,
        base: &mut RgbaImage,
        x: i32,
        y: i32,
        text: &str,
        font_px: f32,
        color: [u8; 4],
    ) {
        let scale = PxScale::from(font_px);
        let mut pen_x = x as f32;
        let (bw, bh) = base.dimensions();
        for ch in text.chars() {
            let font = self.font_for(ch);
            let glyph_id = font.glyph_id(ch);
            let scaled = font.as_scaled(scale);
            let advance = scaled.h_advance(glyph_id).max(font_px * 0.25);
            let glyph = font.outline_glyph(Glyph {
                id: glyph_id,
                scale,
                position: point(pen_x, y as f32 + scaled.ascent()),
            });
            if let Some(glyph) = glyph {
                glyph.draw(|gx, gy, coverage| {
                    if coverage <= 0.02 {
                        return;
                    }
                    let alpha = (coverage * color[3] as f32).round().clamp(0., 255.) as u8;
                    if alpha == 0 {
                        return;
                    }
                    let px = gx as i32;
                    let py = gy as i32;
                    if px < 0 || py < 0 || px >= bw as i32 || py >= bh as i32 {
                        return;
                    }
                    let p = base.get_pixel(px as u32, py as u32);
                    let blend = |fg: u8, bg: u8| {
                        ((fg as u16 * alpha as u16 + bg as u16 * (255 - alpha) as u16) / 255) as u8
                    };
                    base.put_pixel(
                        px as u32,
                        py as u32,
                        image::Rgba([
                            blend(color[0], p[0]),
                            blend(color[1], p[1]),
                            blend(color[2], p[2]),
                            p[3].saturating_add(alpha),
                        ]),
                    );
                });
            }
            pen_x += advance;
        }
    }
}
