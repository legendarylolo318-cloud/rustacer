use eframe::egui::{self, Color32, Pos2, Sense, Shape, Stroke, Ui, Vec2};

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
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
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
        ui.horizontal(|ui| {
            ui.colored_label(
                Color32::from_rgb(0xf5, 0xa6, 0x23),
                "Some actions on this page need administrator rights.",
            );
            if ui.button("Restart as admin").clicked() {
                clicked = true;
            }
        });
        ui.add_space(4.0);
    }
    clicked
}
