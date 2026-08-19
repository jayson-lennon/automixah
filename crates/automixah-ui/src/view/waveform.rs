//! The scrollable, zoomable 3-band waveform view (Mixxx-style).

use eframe::egui;
use eframe::egui::{Color32, Painter, Pos2, Rect, Response, Sense, Vec2};

use crate::audio::peaks::{PeakQuartet, Peaks};

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
    /// Horizontal position of the pinned playhead line, as a fraction of
    /// the viewport width (0.0 = left edge, 0.5 = center, 1.0 = right).
    pub playhead_frac: f32,
}

impl Default for WaveformView {
    fn default() -> Self {
        Self {
            frames_per_pixel: FRAMES_PER_PIXEL_MAX,
            left_frame: 0.0,
            playhead_frac: 0.5,
        }
    }
}

impl WaveformView {
    /// Frames visible across `width_px` pixels.
    #[must_use]
    pub fn visible_frames(&self, width_px: f32) -> f32 {
        self.frames_per_pixel * width_px.max(1.0)
    }

    /// Clamps `left_frame` to one screen of over-scroll on each side, so
    /// the pinned playhead can sit at position 0 or the track end without
    /// detaching from its pin, and the extremes remain reachable by drag.
    pub fn clamp_pan(&mut self, total_frames: f32, width_px: f32) {
        let visible = self.visible_frames(width_px);
        self.left_frame = self.left_frame.clamp(-visible, total_frames);
    }

    /// Zooms by `factor`, keeping `anchor_px` (screen x) fixed on the same
    /// audio frame.
    pub fn zoom_at(&mut self, factor: f32, anchor_px: f32) {
        let anchor_frame = self.left_frame + anchor_px * self.frames_per_pixel;
        self.frames_per_pixel =
            (self.frames_per_pixel * factor).clamp(FRAMES_PER_PIXEL_MIN, FRAMES_PER_PIXEL_MAX);
        self.left_frame = anchor_frame - anchor_px * self.frames_per_pixel;
    }

    /// Places the view so `frame` sits at the pinned playhead position.
    ///
    /// The waveform moves around the playhead:
    /// `left = frame − frac · visible`.
    pub fn pin_frame(&mut self, frame: f32, width_px: f32) {
        let visible = self.visible_frames(width_px);
        self.left_frame = frame - self.playhead_frac * visible;
    }
}

/// Renders the waveform and returns `(response, rect, sample_rate)` — the
/// caller paints overlays onto `rect` in seconds/pixels derived from the view.
pub fn show(
    ui: &mut egui::Ui,
    peaks: &Peaks,
    view: &mut WaveformView,
    pin_frame: Option<f32>,
) -> (Response, Rect, f32) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let mut response = response;
    let painter = ui.painter_at(rect);
    let width = rect.width();
    let total = total_frames(peaks);

    // Unclamped while a drag is in flight: the waveform must be free to
    // move past the track ends so the pinned playhead can reach any
    // position. Clamping returns when the gesture ends.
    let dragging = response.dragged_by(egui::PointerButton::Primary);
    if !dragging {
        view.clamp_pan(total, width);
    }
    handle_input(&mut response, view, rect, pin_frame);
    if !dragging {
        view.clamp_pan(total, width);
    }

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

/// Wheel-zoom (at the pinned playhead while following, at the cursor
/// otherwise) and playhead-pinned follow: the view is placed so the
/// playhead sits at `playhead_frac` of the viewport width.
fn handle_input(
    response: &mut Response,
    view: &mut WaveformView,
    rect: Rect,
    pin_frame: Option<f32>,
) {
    let width = rect.width();
    if let Some(frame) = pin_frame {
        // Following: zoom keeps the pinned playhead fixed, not the cursor.
        let wheel = response.ctx.input(|i| i.raw_scroll_delta.y);
        if wheel != 0.0 {
            view.zoom_at((wheel / 200.0).exp(), view.playhead_frac * width);
        }
        view.pin_frame(frame, width);
    } else if let Some(pos) = response.hover_pos() {
        // Not following (no engine): free zoom at the cursor.
        let anchor_px = pos.x - rect.left();
        let wheel = response.ctx.input(|i| i.raw_scroll_delta.y);
        if wheel != 0.0 {
            view.zoom_at((wheel / 200.0).exp(), anchor_px);
        }
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

/// One layered-RGB column (Mixxx style): each band is a centered column
/// of its own height with alpha, tallest painted first so all bands show.
fn paint_column(painter: &Painter, x: f32, center_y: f32, half_h: f32, q: &PeakQuartet) {
    if q.all == 0 {
        return;
    }
    let heights = band_heights(q, half_h);
    let bands = [
        (heights[0], band_color(Band::Low)),
        (heights[1], band_color(Band::Mid)),
        (heights[2], band_color(Band::High)),
    ];
    let mut order = bands;
    order.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (h, color) in order {
        paint_band(painter, x, center_y, h, color);
    }
}

/// Filled centered column of half-height `h`.
fn paint_band(painter: &Painter, x: f32, center_y: f32, h: f32, color: Color32) {
    if h < 0.5 {
        return;
    }
    painter.rect_filled(
        Rect::from_min_size(Pos2::new(x - 0.5, center_y - h), Vec2::new(1.0, h * 2.0)),
        0.0,
        color,
    );
}

/// Which spectral band a column segment represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Band {
    Low,
    Mid,
    High,
}

/// Band color (semi-transparent so layers blend).
fn band_color(band: Band) -> Color32 {
    match band {
        Band::Low => Color32::from_rgba_premultiplied(180, 30, 30, 160),
        Band::Mid => Color32::from_rgba_premultiplied(30, 180, 60, 140),
        Band::High => Color32::from_rgba_premultiplied(70, 110, 255, 120),
    }
}

/// Stacked band heights (px) from the peak quartet: each band's own value
/// scaled to `half_h`, minimum 1 px when any signal exists.
fn band_heights(q: &PeakQuartet, half_h: f32) -> [f32; 3] {
    let v = |band: u8| {
        let h = half_h * f32::from(band) / 255.0;
        if q.all > 0 { h.max(1.0) } else { h }
    };
    [v(q.low), v(q.mid), v(q.high)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_at(fpp: f32, left: f32) -> WaveformView {
        WaveformView {
            frames_per_pixel: fpp,
            left_frame: left,
            playhead_frac: 0.5,
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

    fn quartet(low: u8, mid: u8, high: u8, all: u8) -> PeakQuartet {
        PeakQuartet {
            low,
            mid,
            high,
            all,
        }
    }

    // Given a low-only quartet at half amplitude.
    // When band heights are computed for half-height 100 px.
    // Then the low band is ~50 px and the others are the 1 px floor.
    #[test]
    fn band_heights_scale_per_band() {
        let [lo, mid, hi] = band_heights(&quartet(128, 0, 0, 128), 100.0);
        assert!((lo - 50.2).abs() < 1.0, "low {lo}");
        assert_eq!(mid, 1.0, "mid floor when silent");
        assert_eq!(hi, 1.0, "high floor when silent");
    }

    // Given a high-dominant quartet.
    // When band heights are computed.
    // Then the high band exceeds the others.
    #[test]
    fn band_heights_high_band_dominates() {
        let [lo, mid, hi] = band_heights(&quartet(10, 20, 255, 255), 80.0);
        assert!(hi > mid && mid > lo, "ordered {lo}/{mid}/{hi}");
        assert!((hi - 80.0).abs() < 1.0, "high reaches full half-height");
    }

    // Given a fully silent quartet.
    // When heights are computed.
    // Then all bands are zero (paint early-outs on q.all == 0 anyway).
    #[test]
    fn band_heights_silent_is_zero() {
        let [lo, mid, hi] = band_heights(&quartet(0, 0, 0, 0), 100.0);
        assert_eq!([lo, mid, hi], [0.0, 0.0, 0.0]);
    }

    // Given the three bands.
    // When colors are assigned.
    // Then each band has a distinct, band-appropriate hue.
    #[test]
    fn band_colors_are_distinct_rgb() {
        let low = band_color(Band::Low);
        let mid = band_color(Band::Mid);
        let high = band_color(Band::High);
        assert!(low.r() > low.g() && low.r() > low.b(), "low is red");
        assert!(mid.g() > mid.r() && mid.g() > mid.b(), "mid is green");
        assert!(high.b() > high.r() && high.b() > high.g(), "high is blue");
        assert_ne!(low, mid);
        assert_ne!(mid, high);
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

    // Given a pinned playhead at 25% of a 200 px viewport.
    // When pinning frame 1000.
    // Then the left edge is 1000 - 0.25 * (fpp * 200).
    #[test]
    fn pin_frame_places_frame_at_frac() {
        let mut view = view_at(10.0, 0.0);
        view.playhead_frac = 0.25;
        view.pin_frame(1000.0, 200.0);
        assert_eq!(view.left_frame, 1000.0 - 0.25 * 10.0 * 200.0);
    }

    // Given a view scrolled past the track end.
    // When clamped.
    // Then the last visible frame is the track end.
    #[test]
    fn clamp_pan_allows_one_screen_overscroll_each_side() {
        // Given a view dragged far past the track end.
        // When clamping.
        // Then it holds at one screen past the end (not snapped to it).
        let mut view = view_at(10.0, 99_999.0);
        view.clamp_pan(4_000.0, 100.0);
        assert_eq!(view.left_frame, 4_000.0, "end over-scroll limit");
        // Given a view dragged far before the start.
        // When clamping.
        // Then it holds at one screen before the start.
        let mut view = view_at(10.0, -99_999.0);
        view.clamp_pan(4_000.0, 100.0);
        assert_eq!(view.left_frame, -1_000.0, "start over-scroll limit");
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
