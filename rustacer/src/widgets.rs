use eframe::egui::{self, Color32, CornerRadius, Pos2, RichText, Sense, Shape, Stroke, Ui, Vec2};

pub fn severity_color(pct: f32) -> Color32 {
    if pct >= 90.0 {
        Color32::from_rgb(0xe5, 0x48, 0x4d)
    } else if pct >= 70.0 {
        Color32::from_rgb(0xf5, 0xa6, 0x23)
    } else {
        Color32::from_rgb(0x3d, 0xb8, 0x6b)
    }
}

/// Circular gauge with a centred percentage and caption underneath.
pub fn gauge(ui: &mut Ui, title: &str, pct: f32, detail: &str) {
    let size = Vec2::new(150.0, 170.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    // Painted shapes are invisible to screen readers; describe the gauge explicitly.
    let spoken = format!("{title}: {:.0} percent. {detail}", pct.clamp(0.0, 100.0));
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ProgressIndicator, true, &spoken)
    });
    let painter = ui.painter_at(rect);
    let center = Pos2::new(rect.center().x, rect.top() + 70.0);
    let radius = 56.0;
    let arc = |from: f32, to: f32| -> Vec<Pos2> {
        let steps = 64;
        (0..=steps)
            .map(|i| {
                let a = from + (to - from) * i as f32 / steps as f32;
                Pos2::new(center.x + radius * a.cos(), center.y + radius * a.sin())
            })
            .collect()
    };
    let start = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let track = ui.visuals().widgets.noninteractive.bg_stroke.color;
    painter.add(Shape::line(
        arc(start, start + sweep),
        Stroke::new(10.0, track),
    ));
    let p = pct.clamp(0.0, 100.0);
    if p > 0.5 {
        painter.add(Shape::line(
            arc(start, start + sweep * p / 100.0),
            Stroke::new(10.0, severity_color(p)),
        ));
    }
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        format!("{p:.0}%"),
        egui::FontId::proportional(24.0),
        ui.visuals().strong_text_color(),
    );
    painter.text(
        Pos2::new(center.x, center.y + radius + 22.0),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(14.0),
        ui.visuals().strong_text_color(),
    );
    painter.text(
        Pos2::new(center.x, center.y + radius + 40.0),
        egui::Align2::CENTER_CENTER,
        detail,
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
}

pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.heading(text);
    ui.add_space(6.0);
}

pub fn admin_banner(ui: &mut Ui, elevated: bool, needed: bool) -> bool {
    let mut clicked = false;
    if needed && !elevated {
        let msg = "Some actions on this page need administrator rights.";
        let color = ui.visuals().warn_fg_color;
        // Side by side when there is room (about 600 points), stacked otherwise.
        if ui.available_width() >= 600.0 {
            ui.horizontal(|ui| {
                ui.colored_label(color, msg);
                clicked = ui.add(primary_btn(ui, "Restart as admin")).clicked();
            });
        } else {
            ui.colored_label(color, msg);
            clicked = ui.add(primary_btn(ui, "Restart as admin")).clicked();
        }
        ui.add_space(4.0);
    }
    clicked
}

/// A horizontal bar filled to `frac` (0..=1) with a text caption beside it, for table cells.
/// The caption carries the value in text, and `spoken` is what screen readers announce.
pub fn size_bar(ui: &mut Ui, frac: f32, caption: &str, spoken: &str) {
    let w = ui.available_width().max(24.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 16.0), Sense::hover());
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ProgressIndicator, true, spoken)
    });
    let text_w = 40.0_f32.min(w * 0.5);
    let bar = egui::Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + 3.0),
        Pos2::new(rect.right() - text_w - 4.0, rect.bottom() - 3.0),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        bar,
        CornerRadius::same(2),
        ui.visuals().widgets.inactive.bg_fill,
    );
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = bar;
        fill.set_width((bar.width() * f).max(2.0).min(bar.width()));
        painter.rect_filled(
            fill,
            CornerRadius::same(2),
            ui.visuals().selection.stroke.color,
        );
    }
    painter.text(
        Pos2::new(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        caption,
        egui::FontId::proportional(12.0),
        ui.visuals().text_color(),
    );
}

const INK: Color32 = Color32::from_rgb(0x10, 0x14, 0x1c);

fn channel(c: u8) -> f32 {
    let c = f32::from(c) / 255.0;
    if c <= 0.039_28 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance.
fn luminance(c: Color32) -> f32 {
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

/// WCAG contrast ratio (1.0 ..= 21.0).
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Pick white or near-black text for `fill`, darkening the fill if neither reaches
/// WCAG AA (4.5:1). Returns (fill, text).
pub fn readable_pair(fill: Color32) -> (Color32, Color32) {
    let mut f = fill;
    for _ in 0..12 {
        let (w, k) = (contrast(f, Color32::WHITE), contrast(f, INK));
        if w >= 4.5 || k >= 4.5 {
            return (f, if w >= k { Color32::WHITE } else { INK });
        }
        f = mix(f, Color32::BLACK, 0.12);
    }
    (f, Color32::WHITE)
}

/// A filled call-to-action button with real hover and press states.
pub struct FilledButton {
    text: String,
    fill: Color32,
}

impl egui::Widget for FilledButton {
    fn ui(self, ui: &mut Ui) -> egui::Response {
        let (fill, ink) = readable_pair(self.fill);
        // Hover moves toward the text colour so it stays visible on light and dark fills.
        let hover = mix(fill, ink, 0.14);
        let press = mix(fill, Color32::BLACK, 0.18);
        ui.scope(|ui| {
            let w = &mut ui.style_mut().visuals.widgets;
            for (state, bg) in [
                (&mut w.inactive, fill),
                (&mut w.hovered, hover),
                (&mut w.active, press),
            ] {
                state.bg_fill = bg;
                state.weak_bg_fill = bg;
                state.bg_stroke = Stroke::new(1.0, mix(bg, ink, 0.25));
                state.fg_stroke = Stroke::new(1.5, ink);
                state.corner_radius = CornerRadius::same(8);
            }
            w.hovered.expansion = 1.5;
            w.active.expansion = 0.5;
            ui.add(
                egui::Button::new(RichText::new(self.text).strong().color(ink))
                    .wrap_mode(egui::TextWrapMode::Extend)
                    .min_size(Vec2::new(96.0, 34.0)),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand)
        })
        .inner
    }
}

/// Accent-coloured call-to-action button.
pub fn primary_btn(ui: &Ui, text: impl Into<String>) -> FilledButton {
    FilledButton {
        text: text.into(),
        fill: ui.visuals().selection.stroke.color,
    }
}

/// Red button for destructive actions.
pub fn danger_btn(_ui: &Ui, text: impl Into<String>) -> FilledButton {
    FilledButton {
        text: text.into(),
        fill: Color32::from_rgb(0xc9, 0x3c, 0x42),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustacer_core::config::Theme;

    #[test]
    fn every_theme_accent_button_is_readable() {
        for t in Theme::ALL {
            let accent = crate::theme::visuals(t).selection.stroke.color;
            let (fill, ink) = readable_pair(accent);
            let c = contrast(fill, ink);
            assert!(c >= 4.5, "{t:?}: contrast {c:.2} for {accent:?}");
            let hover = mix(fill, ink, 0.14);
            assert!(contrast(hover, ink) >= 3.0, "{t:?}: hover contrast");
        }
        let (f, i) = readable_pair(Color32::from_rgb(0xc9, 0x3c, 0x42));
        assert!(contrast(f, i) >= 4.5, "danger button");
    }

    #[test]
    fn contrast_extremes() {
        assert!((contrast(Color32::WHITE, Color32::BLACK) - 21.0).abs() < 0.01);
        assert!((contrast(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 0.01);
    }
}
