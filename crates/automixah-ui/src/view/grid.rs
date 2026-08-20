//! Grid overlay painting + editing controls.

use eframe::egui;
use eframe::egui::{Color32, Painter, Rect};

use crate::grid::EditableGrid;

/// Beat line color: thin translucent white — a sub-tier of the downbeat
/// white and distinct from every RGB waveform band hue (the previous
/// blue was near-identical to the high band and read as waveform).
const BEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(90, 90, 90, 90);
/// Downbeat line color: white, heavier than beats.
const DOWNBEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 220);

/// Beat lines are drawn only when at least this many pixels separate
/// adjacent beats; below it the overlay reads as a solid band of lines
/// and hides the waveform.
const MIN_BEAT_SPACING_PX: f32 = 4.0;

/// White (downbeat) lines are drawn only when at least this many
/// pixels separate adjacent 4-beat groups; below it whites thin out
/// by beat stride (every 4th, 8th, 16th… beat) so the overview stays
/// readable — trance phrases are 4-beat groupings all the way up.
const MIN_WHITE_SPACING_PX: f32 = 50.0;

/// Whether individual beat lines are readable at this zoom: adjacent
/// beats must span [`MIN_BEAT_SPACING_PX`] pixels. Downbeats paint
/// regardless — they stay the phrase-level reference at overview zoom.
fn beat_lines_visible(beat_seconds: f32, seconds_per_pixel: f32) -> bool {
    beat_seconds / seconds_per_pixel.max(f32::EPSILON) >= MIN_BEAT_SPACING_PX
}

/// Smallest stride in 4, 8, 16, 32, … beats whose line spacing still
/// clears [`MIN_WHITE_SPACING_PX`]; doubles without bound because the
/// zoom range far exceeds it.
pub fn white_stride(beat_seconds: f32, seconds_per_pixel: f32) -> u32 {
    let beat_px = beat_seconds / seconds_per_pixel.max(f32::EPSILON);
    let mut stride = 4_u32;
    while stride as f32 * beat_px < MIN_WHITE_SPACING_PX {
        stride = stride.saturating_mul(2);
    }
    stride
}

/// Whether beat `k` starts a white-line group under `stride`:
/// counted relative to the first downbeat so decimated whites
/// always land on bars and share one phrase phase even before
/// the anchor.
fn is_phrase_line(k: i64, stride: u32, downbeat_phase: u8) -> bool {
    (k - i64::from(downbeat_phase)).rem_euclid(i64::from(stride)) == 0
}

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
    let show_beats = beat_lines_visible(beat, seconds_per_pixel);
    let stride = white_stride(beat, seconds_per_pixel);
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
            let show_white = is_downbeat && is_phrase_line(k, stride, grid.downbeat_phase);
            if !show_white && !show_beats {
                k += 1;
                continue;
            }
            let (color, half_w) = if show_white {
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
        ui.label("anchor")
            .on_hover_text("time of the first beat within one bar (the grid repeats every bar)");
        let bar = grid.bar_seconds();
        ui.add_enabled(
            track_end > 0.0,
            egui::Slider::new(&mut grid.anchor_seconds, 0.0..=bar)
                .suffix(" s")
                .custom_formatter(|n, _| format!("{n:.3}")),
        );
    });

    ui.horizontal(|ui| {
        ui.label("shift grid").on_hover_text(
            "moves every beat line left (‹) or right (›) by N milliseconds; wraps within one bar",
        );
        for (label, ms) in [
            ("‹100", -100.0),
            ("‹10", -10.0),
            ("‹1", -1.0),
            ("1›", 1.0),
            ("10›", 10.0),
            ("100›", 100.0),
        ] {
            if ui.small_button(label).clicked() {
                grid.shift_by(ms / 1000.0);
            }
        }
    });

    // Keep the invariant: anchor stays inside [0, bar).
    grid.anchor_seconds = grid.anchor_seconds.rem_euclid(grid.bar_seconds());

    *grid != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    // The waveform band colors (waveform.rs band_color): the grid
    // colors must stay clearly distinct from each of them or beat
    // lines read as waveform content (the original bug: blue beat
    // lines vs the blue high band).
    fn band_colors() -> [Color32; 3] {
        [
            Color32::from_rgba_premultiplied(180, 30, 30, 160),
            Color32::from_rgba_premultiplied(30, 180, 60, 140),
            Color32::from_rgba_premultiplied(70, 110, 255, 120),
        ]
    }

    fn channel_distance(a: Color32, b: Color32) -> f32 {
        let d = |x: u8, y: u8| f32::from(x.abs_diff(y));
        (d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())) / 3.0
    }

    // Given the beat/downbeat line colors and the three waveform band
    // colors.
    // When compared channel-wise.
    // Then each grid color differs from every band color by at least 30
    // average channel levels — distinct over any band at any zoom.
    #[test]
    fn beat_color_distinct_from_band_colors() {
        for band in band_colors() {
            let beat_margin = channel_distance(BEAT_COLOR, band);
            let downbeat_margin = channel_distance(DOWNBEAT_COLOR, band);
            assert!(
                beat_margin >= 30.0,
                "beat color too close to band {band:?} ({beat_margin:.0})"
            );
            assert!(
                downbeat_margin >= 30.0,
                "downbeat color too close to band {band:?} ({downbeat_margin:.0})"
            );
        }
    }

    // Given a beat period and seconds-per-pixel straddling the 4 px
    // visibility threshold.
    // When the visibility rule is evaluated.
    // Then beat lines are hidden below 4 px of spacing and shown at or
    // above it (downbeats paint regardless — separate concern).
    #[test]
    fn beat_lines_hidden_below_min_pixel_spacing() {
        // 138 BPM beat ≈ 0.4348 s.
        let beat = 60.0 / 138.0;
        // Overview zoom on a 48 kHz source: 20000 fpp ≈ 0.4167 s/px →
        // just under one pixel per beat.
        assert!(!beat_lines_visible(beat, 20_000.0 / 48_000.0));
        // Exactly at the threshold is visible.
        assert!(beat_lines_visible(beat, beat / 4.0));
        // Comfortably zoomed in.
    }

    // Given beat spacings across the 50 px white-line threshold.
    // When the stride ladder is evaluated.
    // Then the smallest 4-beat-grouping stride (4, 8, 16, …) whose
    // spacing clears 50 px is chosen.
    #[test]
    fn white_stride_steps_through_beat_groupings() {
        // 138 BPM beat ≈ 0.4348 s.
        let beat = 60.0 / 138.0;
        // A bar (4 beats) clears 50 px → every downbeat.
        assert_eq!(white_stride(beat, 4.0 * beat / 50.0), 4);
        // Overview zoom on a 48 kHz source: 20000 fpp ≈ 0.4167 s/px.
        // Beat ≈ 1.04 px → 64 beats ≈ 67 px → stride 64 (16 bars).
        assert_eq!(white_stride(beat, 20_000.0 / 48_000.0), 64);
        // Exactly at the boundary between 32 and 64: 32 beats = 50 px.
        assert_eq!(white_stride(beat, 32.0 * beat / 50.0), 32);
    }

    // Given stride-16 decimation and a zero downbeat phase.
    // When beat indices are checked for phrase lines.
    // Then every 16th beat from the first downbeat survives — bars
    // in between do not.
    #[test]
    fn phrase_lines_survive_every_16th_beat() {
        assert!(is_phrase_line(0, 16, 0));
        assert!(is_phrase_line(16, 16, 0));
        assert!(is_phrase_line(32, 16, 0));
        assert!(!is_phrase_line(4, 16, 0));
        assert!(!is_phrase_line(8, 16, 0));
        assert!(!is_phrase_line(12, 16, 0));
    }

    // Given stride decimation and a nonzero downbeat phase.
    // When beat indices are checked.
    // Then survivors stay on downbeats and share one phrase phase,
    // including beats before the anchor.
    #[test]
    fn phrase_lines_stay_on_downbeats_for_nonzero_phase() {
        let phase = 2_u8;
        for k in [2_i64, 6, 10, 14] {
            assert!(!is_phrase_line(k, 16, phase) || k == 2);
        }
        assert!(is_phrase_line(2, 16, phase));
        assert!(is_phrase_line(18, 16, phase));
        assert!(!is_phrase_line(6, 16, phase));
        // Pre-anchor beat: −14 is 16 beats before beat 2.
        assert!(is_phrase_line(-14, 16, phase));
        assert!(!is_phrase_line(-10, 16, phase));
    }
}
