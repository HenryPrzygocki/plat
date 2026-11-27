// SPDX-License-Identifier: GPL-3.0-or-later
//! Colours. Everything is defined in OKLCH, so that tiles of different hues
//! have the same perceived lightness and shading a tile is a plain shift of L.

use ratatui::style::Color;

use crate::ext::Cat;

/// A colour in OKLab space.
#[derive(Clone, Copy, Debug)]
pub struct Lab {
    pub l: f32,
    pub a: f32,
    pub b: f32,
}

impl Lab {
    /// From lightness (0..1), chroma and hue in degrees.
    pub fn lch(l: f32, c: f32, h: f32) -> Lab {
        let h = h.to_radians();
        Lab {
            l,
            a: c * h.cos(),
            b: c * h.sin(),
        }
    }

    /// Same hue and chroma, lightness shifted by `dl`.
    pub fn lighten(self, dl: f32) -> Lab {
        Lab {
            l: (self.l + dl).clamp(0.0, 1.0),
            ..self
        }
    }

    /// Linear blend toward `other`; `t = 0` is `self`.
    pub fn mix(self, other: Lab, t: f32) -> Lab {
        Lab {
            l: self.l + (other.l - self.l) * t,
            a: self.a + (other.a - self.a) * t,
            b: self.b + (other.b - self.b) * t,
        }
    }

    fn to_srgb(self) -> (u8, u8, u8) {
        let l = self.l + 0.396_337_78 * self.a + 0.215_803_76 * self.b;
        let m = self.l - 0.105_561_346 * self.a - 0.063_854_17 * self.b;
        let s = self.l - 0.089_484_18 * self.a - 1.291_485_5 * self.b;
        let (l, m, s) = (l * l * l, m * m * m, s * s * s);
        let r = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_93 * s;
        let g = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s;
        let b = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;
        let gamma = |x: f32| {
            let x = x.clamp(0.0, 1.0);
            let v = if x <= 0.003_130_8 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            };
            (v * 255.0).round() as u8
        };
        (gamma(r), gamma(g), gamma(b))
    }
}

/// Nearest colour in the xterm 256-colour palette.
fn to_indexed(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| -> usize {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] as i32 - v as i32).abs())
            .unwrap()
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let dist = |c: (u8, u8, u8)| -> i32 {
        let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2);
        d(c.0, r) + d(c.1, g) + d(c.2, b)
    };
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let gray_step = ((r as i32 + g as i32 + b as i32) / 3 - 8).clamp(0, 230) as u8 / 10;
    let gray_level = 8 + 10 * gray_step.min(23);
    if dist((gray_level, gray_level, gray_level)) < dist(cube) {
        232 + gray_step.min(23)
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

pub struct Theme {
    truecolor: bool,
    /// Application background; treemap gaps show this.
    pub bg: Lab,
    /// Side pane, header and status bar.
    pub panel: Lab,
    /// Selected rows, dialogs.
    pub raised: Lab,
    /// Hairlines and inactive controls.
    pub line: Lab,
    pub text: Lab,
    pub dim: Lab,
    pub faint: Lab,
    /// Dark text for use on bright tiles.
    pub ink: Lab,
    /// Cream highlight: selection, focus, the wordmark.
    pub accent: Lab,
    pub danger: Lab,
    pub danger_dark: Lab,
    pub ok: Lab,
    pub warn: Lab,
    /// Collapsed directory tile.
    pub dir: Lab,
    cats: [Lab; Cat::COUNT],
}

impl Theme {
    pub fn new(truecolor: bool) -> Theme {
        let tile = |h: f32| Lab::lch(0.76, 0.105, h);
        let mut cats = [Lab::lch(0.62, 0.03, 260.0); Cat::COUNT];
        cats[Cat::Other as usize] = Lab::lch(0.64, 0.035, 262.0);
        cats[Cat::Video as usize] = tile(352.0);
        cats[Cat::Audio as usize] = tile(310.0);
        cats[Cat::Image as usize] = Lab::lch(0.82, 0.115, 92.0);
        cats[Cat::Archive as usize] = tile(50.0);
        cats[Cat::Document as usize] = tile(268.0);
        cats[Cat::Code as usize] = tile(182.0);
        cats[Cat::Data as usize] = tile(228.0);
        cats[Cat::Binary as usize] = tile(142.0);
        cats[Cat::Temp as usize] = Lab::lch(0.66, 0.05, 70.0);
        Theme {
            truecolor,
            bg: Lab::lch(0.165, 0.018, 262.0),
            panel: Lab::lch(0.205, 0.02, 262.0),
            raised: Lab::lch(0.27, 0.026, 262.0),
            line: Lab::lch(0.34, 0.025, 262.0),
            text: Lab::lch(0.93, 0.02, 90.0),
            dim: Lab::lch(0.72, 0.02, 262.0),
            faint: Lab::lch(0.53, 0.022, 262.0),
            ink: Lab::lch(0.17, 0.02, 262.0),
            accent: Lab::lch(0.9, 0.075, 88.0),
            danger: Lab::lch(0.66, 0.19, 25.0),
            danger_dark: Lab::lch(0.5, 0.16, 25.0),
            ok: Lab::lch(0.8, 0.13, 150.0),
            warn: Lab::lch(0.82, 0.13, 75.0),
            dir: Lab::lch(0.46, 0.055, 258.0),
            cats,
        }
    }

    /// Pick truecolor unless the terminal clearly cannot do it. `COLORTERM` is
    /// deliberately not required: sudo strips it, and the app should look the
    /// same when run as root.
    pub fn detect() -> Theme {
        let term = std::env::var("TERM").unwrap_or_default();
        let limited = term == "linux" || term == "dumb" || term.starts_with("vt");
        Theme::new(!limited)
    }

    /// Terminal colour for a Lab value.
    pub fn c(&self, lab: Lab) -> Color {
        let (r, g, b) = lab.to_srgb();
        if self.truecolor {
            Color::Rgb(r, g, b)
        } else {
            Color::Indexed(to_indexed(r, g, b))
        }
    }

    pub fn cat(&self, cat: Cat) -> Lab {
        self.cats[cat as usize]
    }

    /// Background of a frame at the given nesting depth.
    pub fn surface(&self, depth: u8) -> Lab {
        if depth == 0 {
            return self.bg;
        }
        let d = depth.min(6) as f32;
        Lab::lch(0.2 + 0.055 * d, 0.024 + 0.004 * d, 262.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_round_trips_known_colours() {
        assert_eq!(Lab::lch(0.0, 0.0, 0.0).to_srgb(), (0, 0, 0));
        assert_eq!(Lab::lch(1.0, 0.0, 0.0).to_srgb(), (255, 255, 255));
        // Mid grey: OKLab L = 0.6 is sRGB 0x80 give or take rounding.
        let (r, g, b) = Lab::lch(0.6, 0.0, 0.0).to_srgb();
        assert!(r == g && g == b && (0x7c..=0x84).contains(&r), "{r}");
    }

    #[test]
    fn indexed_fallback_is_sane() {
        assert_eq!(to_indexed(0, 0, 0), 16);
        assert_eq!(to_indexed(255, 255, 255), 231);
        assert_eq!(to_indexed(255, 0, 0), 196);
        assert!((232..=255).contains(&to_indexed(128, 128, 128)));
    }
}
