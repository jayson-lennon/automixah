//! The transport bar: an always-visible strip along the very bottom of
//! the window that drives the instant preview. Line one names the
//! track ("artist – title", falling back to the file name); line two
//! carries play/pause, elapsed/total time, and a click/drag scrubber
//! spanning the rest of the window width.
//!
//! When no preview exists the bar still renders (a one-line hint), so
//! the layout never jumps. While a decode is in flight it shows the
//! track name and a spinner.

use eframe::egui;

/// The scrubber never shrinks below this; it may clip at the window
/// edge in that pathological case, but nothing overlaps.
const MIN_SCRUBBER_WIDTH: f32 = 96.0;

/// Scrubber strip height in points.
const SCRUBBER_HEIGHT: f32 = 14.0;

/// Actions the transport bar collected this frame.
#[derive(Debug, Default)]
pub struct TransportActions {
    /// Toggle play/pause on the active preview.
    pub toggle_play: bool,
    /// Seek targets (source frames) gathered in gesture order.
    pub seeks: Vec<u64>,
}

/// The bar's headline: "artist – title" when known, title alone when
/// the artist is missing, otherwise the file's base name.
#[must_use]
pub fn track_heading(artist: &str, title: &str, path: &std::path::Path) -> String {
    match (!artist.is_empty(), !title.is_empty()) {
        (true, true) => format!("{artist} \u{2013} {title}"),
        (false, true) => title.to_owned(),
        // Nothing resolvable: fall back to the file name.
        _ => file_label(path),
    }
}

/// A path's base name (empty when the path has none).
#[must_use]
pub fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Renders the transport bar as the outermost bottom panel. Must be
/// registered before any other bottom panel so it paints below them.
///
/// `heading` is line one (track name / hint / decoding status).
pub fn transport_bar(
    ctx: &egui::Context,
    player: Option<&mut crate::audio::preview::PreviewPlayer>,
    heading: &str,
    decoding: bool,
) -> TransportActions {
    let mut actions = TransportActions::default();
    egui::TopBottomPanel::bottom("transport_bar").show(ctx, |ui| {
        ui.add_space(4.0);
        // Line one: what is playing (or the idle hint / decode status).
        ui.horizontal(|ui| {
            if decoding {
                ui.spinner();
            }
            if heading.is_empty() {
                ui.weak("middle-click a playlist row or library entry to preview");
            } else {
                ui.strong(heading);
            }
        });
        let Some(player) = player else {
            return;
        };
        // Line two: the controls, always the same widgets whether a
        // preview runs or not — a paused player still shows position.
        let source_frames = player.source_frames();
        let position = player.position_frames();
        let playing = player.is_audible();
        let sample_rate = player.sample_rate();

        ui.horizontal(|ui| {
            // The whole strip this row gets, captured before any widget
            // shrinks it: the scrubber's width derives from both ends so
            // it can never sweep underneath the fixed widgets ahead of it.
            let row = ui.available_rect_before_wrap();
            let play = ui.button(if playing { "⏸" } else { "▶" });
            if play.clicked() {
                actions.toggle_play = true;
                // The global Space handler ignores keys while any widget
                // holds focus, and a clicked button keeps focus in egui;
                // surrender it or the next Space would be swallowed.
                ui.memory_mut(|m| m.surrender_focus(play.id));
            }
            ui.label(elapsed_label(position, source_frames, sample_rate));

            let total = {
                #[expect(clippy::cast_precision_loss, reason = "frame count to display f64")]
                let frames = source_frames as f64;
                frames
            };
            let spacing = ui.spacing().item_spacing.x;
            let width = (row.right() - ui.cursor().left() - spacing).max(MIN_SCRUBBER_WIDTH);
            scrubber(
                ui,
                egui::Vec2::new(width, SCRUBBER_HEIGHT),
                position,
                total,
                &mut actions.seeks,
            );
        });
        ui.add_space(2.0);
    });
    actions
}

/// "m:ss / m:ss" elapsed label over the full source length.
fn elapsed_label(position_frames: f64, source_frames: u64, sample_rate: u32) -> String {
    let total = {
        #[expect(clippy::cast_precision_loss, reason = "frame count to display f64")]
        let frames = source_frames as f64;
        frames
    };
    format!(
        "{} / {}",
        format_clock(position_frames.max(0.0), sample_rate),
        format_clock(total, sample_rate)
    )
}

/// Frames → "m:ss" display clock at the file's actual sample rate.
fn format_clock(frames: f64, sample_rate: u32) -> String {
    let rate = f64::from(sample_rate.max(1));
    let seconds = (frames.max(0.0) / rate).round() as i64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Maps a fraction along the bar to a source frame, clamped into the
/// valid range. Pure — unit-tested without egui.
#[must_use]
fn frames_from_fraction(fraction: f32, source_frames: f64) -> u64 {
    let clamped = fraction.clamp(0.0, 1.0);
    let frame = f64::from(clamped) * source_frames.max(0.0);
    frame.round().max(0.0) as u64
}

/// The full-width scrubber: shows the position and turns clicks and
/// drags anywhere on it into seek actions.
fn scrubber(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    position: f64,
    source_frames: f64,
    seeks: &mut Vec<u64>,
) {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let frac = if source_frames > 0.0 {
        (position.max(0.0) / source_frames).min(1.0) as f32
    } else {
        0.0
    };
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * frac);
        painter.rect_filled(fill, 3.0, ui.visuals().selection.bg_fill);
    }

    for pointer_x in [
        response.interact_pointer_pos(),
        response.hover_pos().filter(|_| response.clicked()),
    ]
    .into_iter()
    .flatten()
    {
        let fraction = (pointer_x.x - rect.left()) / rect.width().max(1.0);
        seeks.push(frames_from_fraction(fraction, source_frames));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case(-0.5, 88_200.0, 0)]
    #[case(0.0, 88_200.0, 0)]
    #[case(0.25, 88_200.0, 22_050)]
    #[case(0.5, 88_200.0, 44_100)]
    #[case(1.0, 88_200.0, 88_200)]
    #[case(1.7, 88_200.0, 88_200)]
    fn fractions_map_to_clamped_source_frames(
        #[case] fraction: f32,
        #[case] total: f64,
        #[case] expected: u64,
    ) {
        assert_eq!(frames_from_fraction(fraction, total), expected);
    }

    // Given zero-length audio.
    // When any fraction is mapped.
    // Then the target is frame zero (no division blowups).
    #[test]
    fn zero_length_maps_everything_to_frame_zero() {
        assert_eq!(frames_from_fraction(0.7, 0.0), 0);
    }

    // Given tags with both fields.
    // When the headline composes.
    // Then it reads "artist – title".
    #[test]
    fn heading_joins_artist_and_title() {
        let heading = track_heading("Burial", "Archangel", std::path::Path::new("/x.mp3"));

        assert_eq!(heading, "Burial \u{2013} Archangel");
    }

    // Given a missing artist tag.
    // When the headline composes.
    // Then the title stands alone (no dangling dash).
    #[test]
    fn heading_shows_title_without_artist() {
        assert_eq!(
            track_heading("", "Archangel", std::path::Path::new("/x.mp3")),
            "Archangel"
        );
    }

    // Given no usable tags at all.
    // When the headline composes.
    // Then it falls back to the file's base name.
    #[rstest::rstest]
    #[case("/music/03 - hidden gem.wav", "03 - hidden gem.wav")]
    #[case("track.flac", "track.flac")]
    fn heading_falls_back_to_file_name(#[case] path: &str, #[case] expected: &str) {
        assert_eq!(track_heading("", "", std::path::Path::new(path)), expected);
    }

    #[rstest::rstest]
    #[case(0.0, "0:00")]
    #[case(44_099.0, "0:01")] // rounds up across the half-second boundary
    #[case(44_100.0 * 59.6, "1:00")]
    #[case(44_100.0 * 61.0, "1:01")]
    fn clocks_render_minutes_seconds(#[case] frames: f64, #[case] expected: &str) {
        assert_eq!(format_clock(frames, 44_100), expected);
    }

    // Given a 48 kHz file.
    // When the clock renders its length.
    // Then seconds derive from the file's own rate, not a hardcoded one
    // (a 1-minute 48 kHz file must not display as 1:05).
    #[test]
    fn clock_uses_the_files_sample_rate() {
        let minute_at_48k = f64::from(48_000_u32) * 60.0;

        assert_eq!(format_clock(minute_at_48k, 48_000), "1:00");
    }

    // Given the clock fixtures.
    // When the elapsed label composes.
    // Then position and length both appear in order.
    #[test]
    fn elapsed_label_positions_before_total() {
        let label = elapsed_label(0.0, 88_200, 44_100);
        assert!(label.starts_with("0:00 /"));
        assert!(
            label.ends_with("/ 0:02") || label.contains("0:02"),
            "{label}"
        );
    }
}
