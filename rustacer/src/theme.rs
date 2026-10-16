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
        Theme::Dark | Theme::Light => return None,
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
    v.window_corner_radius = CornerRadius::same(12);
    v
}

pub fn visuals(t: Theme) -> Visuals {
    polish(raw_visuals(t))
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
