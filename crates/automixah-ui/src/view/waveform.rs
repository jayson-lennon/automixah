//! The scrollable, zoomable 3-band waveform view (Mixxx-style).

use eframe::egui;
use eframe::egui::{Color32, Painter, Pos2, Rect, Response, Sense, Vec2};

use crate::audio::peaks::{PeakQuartet, Peaks};

/// Mixxx low-band tint: red-leaning.
const LOW_RGB: (u8, u8, u8) = (255, 0, 0);
/// Mixxx mid-band tint: green-leaning.
const MID_RGB: (u8, u8, u8) = (0, 255, 0);
/// Mixxx high-band tint: blue-leaning.
const HIGH_RGB: (u8, u8, u8) = (0, 0, 255);

/// Zoom range in frames per pixel: near-sample level (4) to overview.
pub const FRAMES_PER_PIXEL_MIN: f32 = 4.0;
pub const FRAMES_PER_PIXEL_MAX: f32 = 20_000.0;

/// Interactive waveform view state.
#[derive(Debug, Clone)]
pub struct WaveformView {
    /// Frames per screen pixel at the current zoom (higher = zoomed out).
    pub frames_per_pixel: f32,
    /// Frame index at the left edge of the view.
    pub left_frame: f32,
}

impl Default for WaveformView {
    fn default() -> Self {
        Self {
            frames_per_pixel: FRAMES_PER_PIXEL_MAX,
            left_frame: 0.0,
        }
    }
}

impl WaveformView {
    /// Frames visible across `width_px` pixels.
    #[must_use]
    pub fn visible_frames(&self, width_px: f32) -> f32 {
        self.frames_per_pixel * width_px.max(1.0)
    }

    /// Clamps `left_frame` so the view stays within `[0, total]`.
    pub fn clamp_pan(&mut self, total_frames: f32, width_px: f32) {
        let visible = self.visible_frames(width_px);
        let max_left = (total_frames - visible).max(0.0);
        self.left_frame = self.left_frame.clamp(0.0, max_left);
    }

    /// Zooms by `factor`, keeping `anchor_px` (screen x) fixed on the same
    /// audio frame.
    pub fn zoom_at(&mut self, factor: f32, anchor_px: f32) {
        let anchor_frame = self.left_frame + anchor_px * self.frames_per_pixel;
        self.frames_per_pixel =
            (self.frames_per_pixel * factor).clamp(FRAMES_PER_PIXEL_MIN, FRAMES_PER_PIXEL_MAX);
        self.left_frame = anchor_frame - anchor_px * self.frames_per_pixel;
    }
}

/// Renders the waveform and returns `(response, rect, sample_rate)` — the
/// caller paints overlays onto `rect` in seconds/pixels derived from the view.
pub fn show(
    ui: &mut egui::Ui,
    peaks: &Peaks,
    view: &mut WaveformView,
    center_frame: Option<f32>,
) -> (Response, Rect, f32) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let mut response = response;
    let painter = ui.painter_at(rect);
    let width = rect.width();
    let total = total_frames(peaks);

    view.clamp_pan(total, width);
    handle_input(&mut response, view, rect, center_frame);
    view.clamp_pan(total, width);

    painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
    paint_peaks(&painter, peaks, view, rect);

    let sample_rate = peaks.stride_frames * crate::audio::peaks::VISUAL_RATE;
    (response, rect, sample_rate)
}

/// Total source frames represented by the peak track.
#[must_use]
pub fn total_frames(peaks: &Peaks) -> f32 {
    peaks.data.len() as f32 * peaks.stride_frames
}

/// Wheel-zoom at cursor, drag-to-pan (or scrub), center-follow while playing.
fn handle_input(
    response: &mut Response,
    view: &mut WaveformView,
    rect: Rect,
    center_frame: Option<f32>,
) {
    // No pan branch: scrub subsumes pan; the view follows the playhead.

    if let Some(pos) = response.hover_pos() {
        let anchor_px = pos.x - rect.left();
        let wheel = response.ctx.input(|i| i.raw_scroll_delta.y);
        if wheel != 0.0 {
            view.zoom_at((wheel / 200.0).exp(), anchor_px);
        }
    }

    if let Some(frame) = center_frame {
        let visible = view.visible_frames(rect.width());
        view.left_frame = frame - visible / 2.0;
    }
}

/// Paints per-pixel max-aggregated RGB columns around the vertical center.
fn paint_peaks(painter: &Painter, peaks: &Peaks, view: &WaveformView, rect: Rect) {
    let stride = peaks.stride_frames;
    let total = total_frames(peaks);
    let center_y = rect.center().y;
    let half_h = rect.height() / 2.0;

    let pixels = rect.width().ceil() as usize;
    for px in 0..pixels {
        let x = rect.left() + px as f32 + 0.5;
        let frame_lo = view.left_frame + px as f32 * view.frames_per_pixel;
        let frame_hi = frame_lo + view.frames_per_pixel;
        if frame_hi <= 0.0 || frame_lo >= total {
            continue;
        }
        let column = aggregate(peaks, frame_lo, frame_hi, stride, total);
        paint_column(painter, x, center_y, half_h, &column);
    }
}

/// Max-aggregates every visual sample overlapping `[frame_lo, frame_hi)`.
fn aggregate(peaks: &Peaks, frame_lo: f32, frame_hi: f32, stride: f32, total: f32) -> PeakQuartet {
    let v_lo = (frame_lo.max(0.0) / stride).floor() as isize;
    let v_hi = ((frame_hi.min(total) / stride).ceil() as isize).max(v_lo + 1);
    let mut column = PeakQuartet::default();
    for v in v_lo.max(0)..v_hi.min(peaks.data.len() as isize) {
        let q = peaks.data[v as usize];
        column.low = column.low.max(q.low);
        column.mid = column.mid.max(q.mid);
        column.high = column.high.max(q.high);
        column.all = column.all.max(q.all);
    }
    column
}

/// One RGB-mixed column: height from the overall peak, hue from the band mix.
fn paint_column(painter: &Painter, x: f32, center_y: f32, half_h: f32, q: &PeakQuartet) {
    if q.all == 0 {
        return;
    }
    let amplitude = f32::from(q.all) / 255.0;
    let len = (half_h * amplitude).max(1.0);
    let color = band_mix(q, amplitude);
    painter.rect_filled(
        Rect::from_min_size(
            Pos2::new(x - 0.5, center_y - len),
            Vec2::new(1.0, len * 2.0),
        ),
        0.0,
        color,
    );
}

/// Additive band tint scaled by overall amplitude, clamped per channel.
fn band_mix(q: &PeakQuartet, amplitude: f32) -> Color32 {
    let ch = |band: u8, tint: u8| f32::from(band) * f32::from(tint) / (255.0 * 255.0);
    let r =
        amplitude * 255.0 + ch(q.low, LOW_RGB.0) + ch(q.mid, MID_RGB.0) + ch(q.high, HIGH_RGB.0);
    let g =
        amplitude * 255.0 + ch(q.low, LOW_RGB.1) + ch(q.mid, MID_RGB.1) + ch(q.high, HIGH_RGB.1);
    let b =
        amplitude * 255.0 + ch(q.low, LOW_RGB.2) + ch(q.mid, MID_RGB.2) + ch(q.high, HIGH_RGB.2);
    #[expect(clippy::cast_possible_truncation, reason = "clamped to 255")]
    let byte = |v: f32| v.clamp(0.0, 255.0) as u8;
    Color32::from_rgb(byte(r), byte(g), byte(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_at(fpp: f32, left: f32) -> WaveformView {
        WaveformView {
            frames_per_pixel: fpp,
            left_frame: left,
        }
    }

    fn peaks_with(values: &[u8], stride: f32) -> Peaks {
        Peaks {
            data: values
                .iter()
                .map(|&v| PeakQuartet {
                    low: v,
                    mid: v,
                    high: v,
                    all: v,
                })
                .collect(),
            stride_frames: stride,
        }
    }

    // Given a view zoomed so one pixel spans two visual samples.
    // When a pixel column is aggregated.
    // Then the column is the max of the overlapping samples.
    #[test]
    fn aggregate_takes_max_across_stride() {
        let peaks = peaks_with(&[10, 200, 30, 40], 100.0);
        let column = aggregate(&peaks, 150.0, 350.0, 100.0, 400.0);
        assert_eq!(column.all, 200);
    }

    // Given a zoom pivot at 100 px.
    // When zooming by 2x.
    // Then the frame under the pivot stays fixed.
    #[test]
    fn zoom_keeps_anchor_frame_fixed() {
        let mut view = view_at(10.0, 1000.0);
        let anchor_px = 100.0;
        let anchor_frame_before = view.left_frame + anchor_px * view.frames_per_pixel;
        view.zoom_at(2.0, anchor_px);
        let anchor_frame_after = view.left_frame + anchor_px * view.frames_per_pixel;
        assert!((anchor_frame_before - anchor_frame_after).abs() < 1e-3);
        assert_eq!(view.frames_per_pixel, 20.0);
    }

    // Given a view scrolled past the track end.
    // When clamped.
    // Then the last visible frame is the track end.
    #[test]
    fn clamp_pan_bounds_the_view() {
        let mut view = view_at(10.0, 9_999.0);
        view.clamp_pan(4_000.0, 100.0);
        assert_eq!(view.left_frame, 3_000.0);
    }

    // Given zoom out past the overview limit.
    // When zooming further out.
    // Then frames_per_pixel saturates at the max.
    #[test]
    fn zoom_saturates_at_bounds() {
        let mut view = view_at(FRAMES_PER_PIXEL_MAX, 0.0);
        view.zoom_at(10.0, 50.0);
        assert_eq!(view.frames_per_pixel, FRAMES_PER_PIXEL_MAX);
        view.zoom_at(0.0001, 50.0);
        assert_eq!(view.frames_per_pixel, FRAMES_PER_PIXEL_MIN);
    }
}
