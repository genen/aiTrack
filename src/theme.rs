// SPDX-License-Identifier: GPL-3.0-or-later
//! All colours and style in one place (palette taken from Afterglow's `Theme.cs`).
//! Tweak the constants here to restyle the whole window.

use crate::settings::BadgeVariant;
use crate::vendor::VendorId;
use crate::view::Level;
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Theme, Visuals,
};

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Neutral surfaces and text for one theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub background: Color32,
    pub panel: Color32,
    /// Deeper areas (text inputs, tooltips).
    pub deep: Color32,
    /// Graph / inset areas.
    pub graph: Color32,
    pub line: Color32,
    pub muted: Color32,
    pub foreground: Color32,
    pub button: Color32,
    pub button_border: Color32,
    pub hover: Color32,
    pub selected: Color32,
    // "Phosphor" accents
    pub green: Color32,
    pub purple: Color32,
    pub blue: Color32,
    pub yellow: Color32,
    pub orange: Color32,
    pub red: Color32,
    // Vendor colours
    pub claude: Color32,
    pub codex: Color32,
    pub antigravity: Color32,
    pub grok: Color32,
}

// ---- vendor badges ------------------------------------------------------------------------
//
// The badge (an 18 px rounded square before the vendor's name) is described by a `BadgeStyle`: its
// fill, an optional outline, the mark colour and the corner radius. The mark itself is painted by
// `ui::badge_painter(VendorId)`; a new vendor needs a style here and a painter there. The other
// constants are the base fills of the reference icons for the vendors that may follow (the marks
// are not drawn yet; see README).

/// A badge fill: one colour or a subtle top-to-bottom gradient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fill {
    Solid(Color32),
    Vertical(Color32, Color32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BadgeStyle {
    pub fill: Fill,
    /// 1 px outline in both themes.
    pub border: Option<Color32>,
    /// 1 px outline only in the light theme (white badges would vanish on the light background).
    pub light_border: Option<Color32>,
    /// Colour of the logo.
    pub mark: Color32,
    /// Corner radius as a share of the side.
    pub radius: f32,
}

/// iOS-style squircle corners.
pub const BADGE_RADIUS: f32 = 0.23;
const LIGHT_OUTLINE: Color32 = rgb(0xC9C9CE);

impl BadgeStyle {
    pub const fn new(fill: Fill, mark: Color32) -> Self {
        Self {
            fill,
            border: None,
            light_border: None,
            mark,
            radius: BADGE_RADIUS,
        }
    }

    /// A white-ish badge that gets an outline in the light theme.
    pub const fn outlined(mut self) -> Self {
        self.light_border = Some(LIGHT_OUTLINE);
        self
    }
}

/// The look of `vendor`'s badge in `variant`. `pal` gives the vendor accent used for letter badges
/// (vendors without a logo yet) and the contrasting ink of the filled variant.
///
/// Filled (reference sheet 2): Claude coral gradient with an off-white starburst, Grok near-black
/// with a silver-white mark. Dark (sheet 3, measured): dark gradient squircle, Claude mark coral
/// #DE7857, Grok #08090B with a white mark. Light: #F5F5F5 with a thin #D8D8DC outline, marks in
/// the brand colour (Claude coral #E8694A, Grok near-black).
pub fn badge_style(v: VendorId, variant: BadgeVariant, pal: &Palette) -> BadgeStyle {
    let variant = variant.resolve(pal.dark);
    let dark_fill = Fill::Vertical(rgb(0x313131), rgb(0x14131A));
    let light = |mark| BadgeStyle {
        border: Some(rgb(0xD8D8DC)),
        ..BadgeStyle::new(Fill::Solid(rgb(0xF5F5F5)), mark)
    };
    match (v, variant) {
        (VendorId::Claude, BadgeVariant::Filled) => {
            BadgeStyle::new(Fill::Vertical(rgb(0xEE7654), rgb(0xE5654A)), rgb(0xFFF1E8))
        }
        (VendorId::Claude, BadgeVariant::Dark) => BadgeStyle::new(dark_fill, rgb(0xDE7857)),
        (VendorId::Claude, BadgeVariant::Light) => light(rgb(0xE8694A)),
        (VendorId::Grok, BadgeVariant::Filled) => {
            BadgeStyle::new(Fill::Vertical(rgb(0x0A0A0A), rgb(0x1A1A1A)), rgb(0xE6E6EA))
        }
        (VendorId::Grok, BadgeVariant::Dark) => {
            BadgeStyle::new(Fill::Solid(rgb(0x08090B)), rgb(0xFEFEFE))
        }
        (VendorId::Grok, BadgeVariant::Light) => light(rgb(0x101012)),
        // letter badges: the vendor's accent colour
        (v, BadgeVariant::Filled) => BadgeStyle::new(
            Fill::Solid(pal.vendor(v)),
            if pal.dark {
                pal.background
            } else {
                rgb(0xFFFFFF)
            },
        ),
        (v, BadgeVariant::Dark) => BadgeStyle::new(dark_fill, pal.vendor(v)),
        (v, BadgeVariant::Light) => light(pal.vendor(v)),
        (_, BadgeVariant::Auto) => unreachable!("resolved above"),
    }
}

// Base fills for future vendors (reference icon sheets). Marks are drawn by the vendor's painter.
pub const BADGE_CHATGPT: BadgeStyle =
    BadgeStyle::new(Fill::Solid(rgb(0xF7F7F7)), rgb(0x0D0D0D)).outlined();
pub const BADGE_GEMINI: BadgeStyle =
    BadgeStyle::new(Fill::Solid(rgb(0xFFFFFF)), rgb(0x4285F4)).outlined();
pub const BADGE_COPILOT: BadgeStyle =
    BadgeStyle::new(Fill::Solid(rgb(0xFBEAE0)), rgb(0x7B5CF0)).outlined();
pub const BADGE_PERPLEXITY: BadgeStyle = BadgeStyle::new(Fill::Solid(rgb(0x0B0B0C)), rgb(0xFFFFFF));
pub const BADGE_DEEPSEEK: BadgeStyle = BadgeStyle::new(Fill::Solid(rgb(0x0063E4)), rgb(0xFFFFFF));
pub const BADGE_LLAMA: BadgeStyle =
    BadgeStyle::new(Fill::Vertical(rgb(0x0065FF), rgb(0x7038FB)), rgb(0xFFFFFF));
pub const BADGE_MISTRAL: BadgeStyle =
    BadgeStyle::new(Fill::Vertical(rgb(0xFE6B01), rgb(0xFE3700)), rgb(0xFFFFFF));
pub const BADGE_KLING: BadgeStyle = BadgeStyle::new(Fill::Solid(rgb(0x6E33E3)), rgb(0xFFFFFF));
pub const BADGE_FIREFLY: BadgeStyle = BadgeStyle::new(Fill::Solid(rgb(0xFF0308)), rgb(0xFFFFFF));

pub const DARK: Palette = Palette {
    dark: true,
    background: rgb(0x1C1C1E),
    panel: rgb(0x252527),
    deep: rgb(0x101011),
    graph: rgb(0x171719),
    line: rgb(0x38383A),
    muted: rgb(0xA0A0A5),
    foreground: rgb(0xE8E8EB),
    button: rgb(0x303032),
    button_border: rgb(0x48484C),
    hover: rgb(0x3C3C40),
    selected: rgb(0x28435E),
    green: rgb(0x6BEB5C),
    purple: rgb(0xC76BFA),
    blue: rgb(0x52A8FF),
    yellow: rgb(0xFAD138),
    orange: rgb(0xFF9E2E),
    red: rgb(0xF24747),
    claude: rgb(0xFF9E2E),
    codex: rgb(0x52D6E0),
    antigravity: rgb(0x4794FF),
    grok: rgb(0xD1D1D1),
};

/// Light variant (used when the system prefers light). Accents are darkened for contrast.
pub const LIGHT: Palette = Palette {
    dark: false,
    background: rgb(0xF3F3F5),
    panel: rgb(0xFFFFFF),
    deep: rgb(0xE7E7EB),
    graph: rgb(0xF7F7F9),
    line: rgb(0xD0D0D4),
    muted: rgb(0x5E5E64),
    foreground: rgb(0x1C1C1E),
    button: rgb(0xE8E8EC),
    button_border: rgb(0xC8C8CC),
    hover: rgb(0xE4E4E8),
    selected: rgb(0xD6E6F5),
    green: rgb(0x1E9E3A),
    purple: rgb(0x9B3FD0),
    blue: rgb(0x1F7FE0),
    yellow: rgb(0xB8860B),
    orange: rgb(0xD9730D),
    red: rgb(0xD32F2F),
    claude: rgb(0xD9730D),
    codex: rgb(0x1696A3),
    antigravity: rgb(0x1F6FE0),
    grok: rgb(0x6B6B70),
};

impl Palette {
    pub fn of(theme: Theme) -> &'static Palette {
        match theme {
            Theme::Dark => &DARK,
            Theme::Light => &LIGHT,
        }
    }

    /// The vendor's own colour (stripe, badge, lit segments, percentage).
    pub fn vendor(&self, v: VendorId) -> Color32 {
        match v {
            VendorId::Claude => self.claude,
            VendorId::Codex => self.codex,
            VendorId::Antigravity => self.antigravity,
            VendorId::Grok => self.grok,
        }
    }

    /// Strip + value colour: the vendor colour, or red from 90 %.
    pub fn level(&self, v: VendorId, l: Level) -> Color32 {
        match l {
            Level::Normal => self.vendor(v),
            Level::Bad => self.red,
        }
    }

    pub fn dot(&self, d: crate::view::Dot) -> Color32 {
        match d {
            crate::view::Dot::Ok => self.green,
            crate::view::Dot::Stale => self.orange,
            crate::view::Dot::Err => self.red,
        }
    }
}

/// Alpha of unlit LED segments (same hue as lit ones).
pub const UNLIT_ALPHA: f32 = 0.14;
/// Alpha of segments that show the projected usage at reset.
pub const PROJECTED_ALPHA: f32 = 0.35;
/// The same two alphas on the light theme: pale tints over white need more weight to stay visible.
pub const LIGHT_UNLIT_ALPHA: f32 = 0.22;
pub const LIGHT_PROJECTED_ALPHA: f32 = 0.5;
/// Bloom: faint, wider copies painted behind lit segments (expand in px, alpha).
pub const BLOOM: [(f32, f32); 2] = [(2.5, 0.10), (1.2, 0.20)];

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.background;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.deep;
    v.faint_bg_color = p.panel;
    v.window_stroke = Stroke::new(1.0_f32, p.line);
    v.override_text_color = None;
    v.hyperlink_color = p.blue;
    v.selection.bg_fill = p.selected;
    v.selection.stroke = Stroke::new(1.0_f32, p.blue);
    let r = CornerRadius::same(5);
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.foreground);
    v.widgets.noninteractive.corner_radius = r;
    for (w, fill) in [
        (&mut v.widgets.inactive, p.button),
        (&mut v.widgets.hovered, p.hover),
        (&mut v.widgets.active, p.hover),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0_f32, p.button_border);
        w.fg_stroke = Stroke::new(1.0_f32, p.foreground);
        w.corner_radius = r;
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.muted);
    v.widgets.open = v.widgets.hovered;
    v
}

/// Install visuals for both themes, follow the system light/dark preference, load fonts.
pub fn install(ctx: &egui::Context) {
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT));
    ctx.set_theme(egui::ThemePreference::System);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
    });
    install_fonts(ctx);
}

/// Prefer Segoe UI / Consolas (and Segoe UI Symbol for glyphs like the check marks) when present;
/// egui's bundled fonts remain the fallback, so this is harmless on other systems.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map(|w| std::path::PathBuf::from(w).join("Fonts"))
        .unwrap_or_else(|| "C:\\Windows\\Fonts".into());
    let mut add = |file: &str, name: &str, family: FontFamily, first: bool| {
        if let Ok(bytes) = std::fs::read(dir.join(file)) {
            fonts.font_data.insert(
                name.to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
            let list = fonts.families.entry(family).or_default();
            if first {
                list.insert(0, name.to_owned());
            } else {
                list.push(name.to_owned());
            }
        }
    };
    add("segoeui.ttf", "segoe-ui", FontFamily::Proportional, true);
    add("consola.ttf", "consolas", FontFamily::Monospace, true);
    add(
        "seguisym.ttf",
        "segoe-symbols",
        FontFamily::Proportional,
        false,
    );
    add(
        "seguisym.ttf",
        "segoe-symbols-m",
        FontFamily::Monospace,
        false,
    );
    ctx.set_fonts(fonts);
}
