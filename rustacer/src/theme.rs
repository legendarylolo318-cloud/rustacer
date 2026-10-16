use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};
use rustacer_core::config::Theme;

struct Palette {
    dark: bool,
    bg: Color32,
    panel: Color32,
    widget: Color32,
    hover: Color32,
    accent: Color32,
    text: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn palette(t: Theme) -> Option<Palette> {
    Some(match t {
        Theme::Dark | Theme::Light | Theme::HighContrast => return None,
        Theme::Midnight => Palette {
            dark: true,
            bg: rgb(0x0b, 0x10, 0x1c),
            panel: rgb(0x11, 0x18, 0x2a),
            widget: rgb(0x1c, 0x26, 0x40),
            hover: rgb(0x28, 0x36, 0x5c),
            accent: rgb(0x4d, 0x8d, 0xff),
            text: rgb(0xd7, 0xe0, 0xf5),
        },
        Theme::Nord => Palette {
            dark: true,
            bg: rgb(0x2e, 0x34, 0x40),
            panel: rgb(0x3b, 0x42, 0x52),
            widget: rgb(0x43, 0x4c, 0x5e),
            hover: rgb(0x4c, 0x56, 0x6a),
            accent: rgb(0x88, 0xc0, 0xd0),
            text: rgb(0xec, 0xef, 0xf4),
        },
        Theme::Dracula => Palette {
            dark: true,
            bg: rgb(0x28, 0x2a, 0x36),
            panel: rgb(0x34, 0x37, 0x46),
            widget: rgb(0x44, 0x47, 0x5a),
            hover: rgb(0x58, 0x5b, 0x73),
            accent: rgb(0xbd, 0x93, 0xf9),
            text: rgb(0xf8, 0xf8, 0xf2),
        },
        Theme::Solarized => Palette {
            dark: true,
            bg: rgb(0x00, 0x2b, 0x36),
            panel: rgb(0x07, 0x36, 0x42),
            widget: rgb(0x0f, 0x45, 0x52),
            hover: rgb(0x1a, 0x58, 0x66),
            accent: rgb(0x2a, 0xa1, 0x98),
            text: rgb(0xee, 0xe8, 0xd5),
        },
        Theme::Forest => Palette {
            dark: true,
            bg: rgb(0x14, 0x1c, 0x17),
            panel: rgb(0x1b, 0x27, 0x20),
            widget: rgb(0x26, 0x38, 0x2d),
            hover: rgb(0x33, 0x4d, 0x3c),
            accent: rgb(0x6c, 0xc0, 0x7a),
            text: rgb(0xdd, 0xec, 0xdf),
        },
    })
}

/// Rounded controls with a subtle outline, shared by every theme.
fn polish(mut v: Visuals) -> Visuals {
    let r = CornerRadius::same(8);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, v.widgets.inactive.bg_fill.gamma_multiply(1.5));
    // Keyboard focus (and press) gets a clear 2 px accent outline.
    v.widgets.active.bg_stroke = Stroke::new(2.0, v.selection.stroke.color);
    v.window_corner_radius = CornerRadius::same(12);
    v
}

/// Black / white / yellow scheme aimed at WCAG AAA text contrast.
fn high_contrast() -> Visuals {
    let black = Color32::BLACK;
    let white = Color32::WHITE;
    let yellow = rgb(0xff, 0xd6, 0x00);
    let blue = rgb(0x00, 0x37, 0xa6);
    let mut v = Visuals::dark();
    v.override_text_color = Some(white);
    v.weak_text_color = Some(rgb(0xe6, 0xe6, 0xe6));
    v.panel_fill = black;
    v.window_fill = black;
    v.extreme_bg_color = black;
    v.faint_bg_color = rgb(0x14, 0x14, 0x14);
    v.window_stroke = Stroke::new(2.0, white);
    v.hyperlink_color = yellow;
    v.selection.bg_fill = blue;
    v.selection.stroke = Stroke::new(2.0, yellow);
    v.warn_fg_color = yellow;
    v.error_fg_color = rgb(0xff, 0x8a, 0x8a);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.bg_fill = black;
        w.weak_bg_fill = black;
        w.fg_stroke = Stroke::new(1.5, white);
        w.bg_stroke = Stroke::new(1.5, white);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, rgb(0xb0, 0xb0, 0xb0));
    // bg_fill draws slider tracks and checkbox interiors: keep them visible on black.
    v.widgets.inactive.bg_fill = rgb(0x5a, 0x5a, 0x5a);
    v.widgets.hovered.bg_fill = rgb(0x70, 0x70, 0x70);
    v.widgets.active.bg_fill = yellow;
    v.widgets.hovered.bg_stroke = Stroke::new(2.0, yellow);
    v.widgets.hovered.weak_bg_fill = rgb(0x1a, 0x1a, 0x1a);
    v.widgets.active.bg_stroke = Stroke::new(2.0, yellow);
    v
}

pub fn visuals(t: Theme) -> Visuals {
    if t == Theme::HighContrast {
        // polish() would soften the strokes; keep the high-contrast ones.
        let mut v = high_contrast();
        v.window_corner_radius = CornerRadius::same(4);
        return v;
    }
    polish(raw_visuals(t))
}

/// Turn animations off (or back to egui's default).
pub fn apply_motion(ctx: &egui::Context, reduce: bool) {
    ctx.global_style_mut(|s| {
        s.animation_time = if reduce { 0.0 } else { 1.0 / 12.0 };
        s.scroll_animation = if reduce {
            egui::style::ScrollAnimation::none()
        } else {
            egui::style::ScrollAnimation::default()
        };
    });
}

fn raw_visuals(t: Theme) -> Visuals {
    let Some(p) = palette(t) else {
        return if t == Theme::Light {
            Visuals::light()
        } else {
            Visuals::dark()
        };
    };
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.override_text_color = Some(p.text);
    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.bg;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.linear_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.widget);
    v.widgets.inactive.bg_fill = p.widget;
    v.widgets.inactive.weak_bg_fill = p.widget;
    v.widgets.hovered.bg_fill = p.hover;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.bg_fill = p.accent;
    v.widgets.active.weak_bg_fill = p.accent;
    v
}

/// Larger, easier-to-hit controls; applied once at startup.
pub fn apply_sizing(ctx: &egui::Context) {
    ctx.global_style_mut(|s| {
        s.spacing.button_padding = egui::vec2(14.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.icon_width = 20.0;
        s.spacing.icon_width_inner = 12.0;
        for (ts, size) in [
            (egui::TextStyle::Body, 15.0),
            (egui::TextStyle::Button, 15.0),
            (egui::TextStyle::Heading, 24.0),
        ] {
            if let Some(f) = s.text_styles.get_mut(&ts) {
                f.size = size;
            }
        }
        let r = CornerRadius::same(6);
        s.visuals.widgets.inactive.corner_radius = r;
        s.visuals.widgets.hovered.corner_radius = r;
        s.visuals.widgets.active.corner_radius = r;
    });
}
