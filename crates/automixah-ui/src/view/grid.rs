//! Grid overlay painting + editing controls.

use eframe::egui;
use eframe::egui::{Color32, Painter, Rect};

use crate::grid::EditableGrid;

/// Beat line tint (light).
const BEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(220, 220, 220, 40);
/// Downbeat line tint (heavy, warm).
const DOWNBEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(255, 170, 0, 110);

/// Draws beat/downbeat lines for `grid` over the visible span of `rect`.
///
/// `time_at_left` / `seconds_per_pixel` come from the waveform view; lines
/// outside the rect are skipped by the pixel loop below.
pub fn paint(
    painter: &Painter,
    grid: &EditableGrid,
    rect: Rect,
    seconds_per_pixel: f32,
    time_at_left: f32,
    track_end: f32,
) {
    let beat = grid.beat_seconds();
    // First beat index whose time could appear in view.
    let first_k = ((time_at_left - grid.anchor_seconds) / beat).floor() as i64 - 1;
    let mut k = first_k;
    loop {
        let time = grid.anchor_seconds + k as f32 * beat;
        if time > track_end || time > time_at_left + rect.width() * seconds_per_pixel + beat {
            if time > time_at_left + rect.width() * seconds_per_pixel + beat {
                break;
            }
            k += 1;
            continue;
        }
        if time < 0.0 {
            k += 1;
            continue;
        }
        let x = rect.left() + (time - time_at_left) / seconds_per_pixel;
        if (rect.left()..=rect.right()).contains(&x) {
            let is_downbeat =
                k.rem_euclid(crate::grid::BEATS_PER_BAR as i64) == i64::from(grid.downbeat_phase);
            let (color, half_w) = if is_downbeat {
                (DOWNBEAT_COLOR, 1.0)
            } else {
                (BEAT_COLOR, 0.5)
            };
            let top = rect.top();
            let h = rect.height();
            painter.rect_filled(
                Rect::from_min_size((x - half_w, top).into(), egui::vec2(half_w * 2.0, h)),
                0.0,
                color,
            );
        }
        k += 1;
    }
}

/// Renders the editing panel for `grid`; returns true when any value changed.
pub fn controls(ui: &mut egui::Ui, grid: &mut EditableGrid, track_end: f32) -> bool {
    let before = *grid;

    ui.horizontal(|ui| {
        ui.label("BPM");
        ui.add(
            egui::DragValue::new(&mut grid.grid_bpm)
                .speed(0.01)
                .range(20.0..=300.0),
        );
    });

    ui.horizontal(|ui| {
        ui.label("anchor");
        ui.add_enabled(
            track_end > 0.0,
            egui::Slider::new(&mut grid.anchor_seconds, 0.0..=track_end).suffix(" s"),
        );
    });

    ui.horizontal(|ui| {
        ui.label("nudge");
        for (label, ms) in [
            ("−100", -100.0),
            ("−10", -10.0),
            ("−1", -1.0),
            ("+1", 1.0),
            ("+10", 10.0),
            ("+100", 100.0),
        ] {
            if ui.small_button(label).clicked() {
                grid.anchor_seconds = (grid.anchor_seconds + ms / 1000.0).max(0.0);
            }
        }
    });

    // Keep the invariant: anchor stays inside [0, bar).
    grid.anchor_seconds = grid.anchor_seconds.rem_euclid(grid.bar_seconds());

    *grid != before
}
