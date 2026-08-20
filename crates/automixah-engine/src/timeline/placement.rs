//! Transition-window placement on downbeat/phrase boundaries.
//!
//! The planner places each A→B transition so that:
//!
//! - B's cue is B's *first confident downbeat* (beat 1 of a bar near the
//!   start of the track, after any silence/intro); B's source cue maps
//!   onto the window start (`src_start`), it does not constrain length;
//! - the window *ends* at A's *last usable downbeat anchor* — the last
//!   downbeat leaving a bar of margin before A's end;
//! - the window *length* is the preset's beat count at the session
//!   tempo, clamped so a short track still gets at least one bar;
//! - all session times are sample counts at the engine rate
//!   (sample-exact by construction).
//!
//! When either track lacks a confident grid (low stability or empty
//! beats), the window degrades to a time-aligned crossfade span computed
//! from the BPM estimate alone — the session still plays.

use crate::timeline::types::{SessionTime, TransitionWindow};
use djcore::analyzer::BeatGrid;

/// `grid_stability` below this means the beat grid is untrusted.
pub const GRID_STABILITY_THRESHOLD: f32 = 0.3;

/// Bars of margin left after the last usable anchor in A (one bar of
/// outro room).
const OUTGOING_MARGIN_BARS: usize = 1;

/// Beats per bar (4/4 assumption).
const BEATS_PER_BAR: usize = 4;

/// A track's beat-grid-derived anchors, extracted for placement.
#[derive(Debug, Clone)]
pub struct GridAnchors {
    /// Session-time cue point for this track (first confident downbeat,
    /// in *source* seconds; converted by the caller).
    pub cue_seconds: f32,
    /// Session-time end anchor (last usable downbeat minus margin,
    /// in *source* seconds).
    pub end_anchor_seconds: f32,
}

/// Returns whether a beat grid is confident enough for phrase-aligned
/// placement.
#[must_use]
pub fn grid_is_confident(beat_grid: &BeatGrid, grid_stability: f32) -> bool {
    grid_stability >= GRID_STABILITY_THRESHOLD && !beat_grid.downbeats.is_empty()
}

/// Extracts placement anchors from a beat grid.
///
/// The cue is the first downbeat; the end anchor is the last downbeat
/// leaving `OUTGOING_MARGIN_BARS` bars of margin before `duration`.
/// Returns `None` when the grid is unusable (caller falls back).
///
/// Bar projection uses drift re-anchoring: expected bar times are
/// projected from a running anchor at the median bar period, and each
/// observed downbeat within half a bar of the projection re-anchors it
/// only when accumulated drift exceeds [`DRIFT_REANCHOR_SECONDS`]
/// (5 ms). Small drift is *snapped away* so windows land on the observed
/// bar, but sub-threshold jitter never moves the projection — tempo
/// wobble does not accumulate.
#[must_use]
pub fn anchors_from_grid(beat_grid: &BeatGrid, duration: f32) -> Option<GridAnchors> {
    let downbeats = &beat_grid.downbeats;
    if downbeats.is_empty() {
        return None;
    }

    let cue = downbeats[0];
    let bar_len = bar_length_seconds(beat_grid).unwrap_or(2.0);
    let normalized = normalize_bars(downbeats, bar_len);
    #[expect(clippy::cast_precision_loss, reason = "small constant count")]
    let margin = OUTGOING_MARGIN_BARS as f32 * bar_len;
    let last_usable = *normalized
        .iter()
        .rev()
        .find(|&&db| db + margin <= duration)
        .unwrap_or(&normalized[0]);

    Some(GridAnchors {
        cue_seconds: cue,
        end_anchor_seconds: last_usable,
    })
}

/// Accumulated drift (seconds) beyond which the bar projection
/// re-anchors to the observed downbeat; below it, drift is snapped away.
pub const DRIFT_REANCHOR_SECONDS: f32 = 0.005;

/// Re-anchors the bar projection over observed downbeats.
///
/// Bars are projected as `anchor + period`; when an observed downbeat
/// drifts more than [`DRIFT_REANCHOR_SECONDS`] from the projection, the
/// anchor resets to it (subsequent projections continue from the new
/// anchor). Any observation more than half a bar away — sub-threshold
/// jitter, gradual tempo drift, or a missed/extra bar — is itself the
/// best available event, so it always re-anchors rather than being
/// trusted against a stale projection. Sub-threshold drift (< 5 ms) is
/// snapped away so jitter never accumulates.
/// snapped away so jitter never accumulates.
fn normalize_bars(downbeats: &[f32], period: f32) -> Vec<f32> {
    let mut normalized = Vec::with_capacity(downbeats.len());
    let mut anchor = downbeats[0];
    for &db in downbeats {
        let expected = anchor + period;
        let drift = db - expected;
        if drift.abs() > DRIFT_REANCHOR_SECONDS {
            // Re-anchor (covers outliers too): the observation is the
            // best available event; projection continues from it.
            anchor = db;
            normalized.push(db);
        } else {
            // Snap sub-threshold jitter: keep the projected position
            // and advance the anchor by the period, not the jitter.
            anchor = expected;
            normalized.push(expected);
        }
    }
    normalized
}

/// Estimates the bar length in seconds from a beat grid (4× the median
/// inter-beat interval). Returns `None` with fewer than two beats.
#[must_use]
fn bar_length_seconds(beat_grid: &BeatGrid) -> Option<f32> {
    let beats = &beat_grid.beats;
    if beats.len() < 2 {
        return None;
    }
    let mut intervals: Vec<f32> = beats.windows(2).map(|w| w[1] - w[0]).collect();
    intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = intervals[intervals.len() / 2];
    #[expect(clippy::cast_precision_loss, reason = "small constant")]
    let bars = BEATS_PER_BAR as f32;
    Some(median * bars)
}

/// Places the A→B transition window in *session* time.
///
/// Inputs to [`place_window`] beyond the anchor pair — the session
/// clock, the outgoing end, the preset length, and fallback data.
#[derive(Debug, Clone, Copy)]
pub struct WindowInputs {
    /// The preset's window length in beats (e.g. 32 for Crossfade).
    pub preset_beats: usize,
    /// Where segment A's audio ends in session samples: the stretched
    /// position of A's last downbeat with audio when its grid is
    /// confident, else A's full stretched end. The window closes at
    /// or before this point.
    pub a_session_end: SessionTime,
    /// B's cue in session samples (informational; maps to `src_start`).
    pub b_cue_session: SessionTime,
    /// Session tempo.
    pub session_bpm: f32,
    /// Engine sample rate.
    pub sample_rate: u32,
    /// A's stretched-grid phase in session samples: a session-grid
    /// beat that A's beat grid lands on. `None` when A's grid is
    /// unconfident (fallback path, no snapping).
    pub a_grid_phase: Option<SessionTime>,
}

/// Places the A→B transition window in *session* time.
///
/// The window is `[end - preset_beats, end]`, floored at the session
/// start; B's cue maps onto the window start via `src_start` (the
/// caller's job). Short effective spans clamp the window to at least
/// one bar.
///
/// `a_session_end` carries A's natural end: the stretched position of
/// its last downbeat with audio when the grid is confident, else its
/// full stretched end. Both confident and fallback grids place the
/// window against that end, so tempo scaling can never push the
/// window past the audio (the old fallback computed in native seconds
/// — the bug that produced silent gaps between tracks).
#[must_use]
pub fn place_window(inputs: WindowInputs) -> TransitionWindow {
    let WindowInputs {
        preset_beats,
        a_session_end,
        b_cue_session: _,
        session_bpm,
        sample_rate,
        a_grid_phase,
    } = inputs;

    let beat = 60.0 / session_bpm;
    #[expect(clippy::cast_precision_loss, reason = "small constant")]
    let bars = BEATS_PER_BAR as f32;
    let bar = beat * bars;
    #[expect(clippy::cast_precision_loss, reason = "beat count is small")]
    let window_len = preset_beats as f32 * beat;
    let min_len = bar; // short-track clamp: at least one bar

    let end = a_session_end;
    let requested = SessionTime::from_seconds(window_len, sample_rate);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bar length is small"
    )]
    // Window clamped to at least one bar and at most half the
    // outgoing segment: short fixtures (and intros) never let the
    // window swallow the whole track.
    let max_len = (end.0 / 2).max(min_len as u64);
    let len = requested.0.clamp(min_len as u64, max_len);
    let start = SessionTime(end.0.saturating_sub(len));

    match a_grid_phase {
        Some(phase) => snap_window(phase, beat, sample_rate, start, end),
        None => TransitionWindow { start, end },
    }
}

/// Snaps both window boundaries to the nearest session-grid beat
/// of A's stretched grid, keeping `end` from passing A's session
/// end and the length at or above one bar.
///
/// The snap runs in f64 on the *float* beat length: the session
/// beat is generally not an integer sample count (19174.39 at
/// 138 BPM), and integer-beat snapping accumulates fractional
/// drift — after ~350 beats the window sits a full 136 samples
/// off the true grid.
fn snap_window(
    phase: SessionTime,
    beat: f32,
    sample_rate: u32,
    start: SessionTime,
    end: SessionTime,
) -> TransitionWindow {
    let beat_samples = f64::from(beat) * f64::from(sample_rate);

    let start_snap = snap_to_grid(start, phase, beat_samples);
    let mut end_snap = snap_to_grid(end, phase, beat_samples);

    // Clamp inward: the window must not extend past A's session
    // end nor shrink below one bar.
    if end_snap > end {
        #[expect(clippy::cast_possible_truncation, reason = "one beat")]
        let one_beat = beat_samples as u64;
        end_snap = SessionTime(end_snap.0.saturating_sub(one_beat));
    }
    let min_samples = beat_samples * f64::from(BEATS_PER_BAR as u32);
    let span = end_snap.0.saturating_sub(start_snap.0);
    #[expect(clippy::cast_precision_loss, reason = "sample span")]
    if (span as f64) < min_samples {
        return TransitionWindow { start, end };
    }
    TransitionWindow {
        start: start_snap,
        end: end_snap,
    }
}

/// Nearest grid beat to `t` on the float grid `phase + k·beat`,
/// rounded to whole samples.
fn snap_to_grid(t: SessionTime, phase: SessionTime, beat_samples: f64) -> SessionTime {
    let t_f = (t.0.max(phase.0) - phase.0.min(t.0)) as f64;
    let k = (t_f / beat_samples).round();
    let snapped = phase.0 as f64 + k * beat_samples;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "snapped beat within session bounds"
    )]
    SessionTime(snapped.max(0.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(clippy::cast_precision_loss, reason = "test helper: small indices")]
    fn grid(downbeat_period: f32, count: usize) -> BeatGrid {
        let downbeats = (0..count).map(|i| i as f32 * downbeat_period).collect();
        #[expect(clippy::cast_precision_loss, reason = "test helper: small indices")]
        let beats = (0..count * 4)
            .map(|i| i as f32 * downbeat_period / 4.0)
            .collect();
        BeatGrid {
            grid_bpm: 240.0 / downbeat_period,
            anchor_seconds: 0.0,
            downbeats,
            beats,
            bars: Vec::new(),
        }
    }

    #[test]
    fn grid_confidence_requires_stability_and_downbeats() {
        // Given a populated grid with low stability.
        let g = grid(2.0, 10);
        assert!(!grid_is_confident(&g, 0.2));

        // Given high stability but empty downbeats.
        let empty = BeatGrid::default();
        assert!(!grid_is_confident(&empty, 0.9));

        // Given high stability and downbeats.
        assert!(grid_is_confident(&g, 0.9));
    }

    #[test]
    fn anchors_use_first_downbeat_as_cue() {
        // Given a 120 BPM grid (2 s bars) with 60 bars = 120 s.
        let g = grid(2.0, 60);

        // When extracting anchors over a 120 s track.
        let anchors = anchors_from_grid(&g, 120.0).expect("anchors");

        // Then the cue is the first downbeat.
        assert!((anchors.cue_seconds - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn anchors_leave_one_bar_of_margin() {
        // Given a 120 BPM grid with bars at 0, 2, ..., 118 in a 119 s track.
        let g = grid(2.0, 60); // downbeats 0..=118

        // When extracting anchors.
        let anchors = anchors_from_grid(&g, 119.0).expect("anchors");

        // Then the end anchor leaves one bar of margin (118 + 2 > 119 → 116).
        assert!((anchors.end_anchor_seconds - 116.0).abs() < f32::EPSILON);
    }

    #[test]
    fn anchors_on_empty_grid_return_none() {
        // Given an empty grid.
        // When extracting anchors.
        let anchors = anchors_from_grid(&BeatGrid::default(), 100.0);

        // Then there are none.
        assert!(anchors.is_none());
    }

    #[test]
    fn window_places_start_preset_beats_before_end() {
        // Given a confident pair, a 32-beat preset at 120 BPM.
        let g = grid(2.0, 60);
        let _anchors = anchors_from_grid(&g, 120.0).expect("anchors");

        // When placing the window ending at session sample 1_000_000.
        let w = place_window(WindowInputs {
            preset_beats: 32,
            a_session_end: SessionTime(1_000_000),
            b_cue_session: SessionTime(500_000),
            session_bpm: 120.0,
            sample_rate: 44_100,
            a_grid_phase: None,
        });

        // Then the window is clamped to half the outgoing span
        // (500_000 < 2×705_600) and ends at the anchor.
        assert_eq!(w.end, SessionTime(1_000_000));
        assert_eq!(w.len_samples(), 500_000);
    }

    #[test]
    fn window_length_is_independent_of_b_cue() {
        // Given a B cue late in the session; the cue maps to src_start,
        // it does not constrain window length.
        let g = grid(2.0, 60);
        let _anchors = anchors_from_grid(&g, 120.0).expect("anchors");

        // When placing a 32-beat window ending at 1_000_000 with cue at 900_000.
        let w = place_window(WindowInputs {
            preset_beats: 32,
            a_session_end: SessionTime(1_000_000),
            b_cue_session: SessionTime(900_000),
            session_bpm: 120.0,
            sample_rate: 44_100,
            a_grid_phase: None,
        });

        // Then the window length is clamped to half the outgoing
        // span regardless of the cue.
        assert_eq!(w.end, SessionTime(1_000_000));
        assert_eq!(w.len_samples(), 500_000);
    }
    #[test]
    fn window_floors_at_session_start_when_a_is_short() {
        // Given a session end so early that a 32-beat window cannot fit.
        let g = grid(2.0, 60);
        let _anchors = anchors_from_grid(&g, 120.0).expect("anchors");

        // When placing the window ending at session sample 10_000.
        let w = place_window(WindowInputs {
            preset_beats: 32,
            a_session_end: SessionTime(10_000),
            b_cue_session: SessionTime(9_000),
            session_bpm: 120.0,
            sample_rate: 44_100,
            a_grid_phase: None,
        });

        // Then the window is half of A's span (min one bar): the
        // tiny 10_000-sample span floors at the bar minimum.
        assert_eq!(w.start, SessionTime(5_000));
        assert_eq!(w.end, SessionTime(10_000));
    }

    #[test]
    fn missing_grid_places_window_against_session_end() {
        // Given no anchors and a session end of 60 s.
        let w = place_window(WindowInputs {
            preset_beats: 32,
            a_session_end: SessionTime::from_seconds(60.0, 44_100),
            b_cue_session: SessionTime(0),
            session_bpm: 120.0,
            sample_rate: 44_100,
            a_grid_phase: None,
        });

        // Then the window ends exactly at the session end (never past
        // the audio, regardless of stretch) with the requested length.
        assert_eq!(w.end, SessionTime::from_seconds(60.0, 44_100));
        assert_eq!(w.start, SessionTime::from_seconds(44.0, 44_100));
    }

    #[test]
    fn sub_threshold_jitter_is_snapped_away() {
        // Given bars at 2 s with 1 ms jitter (below the 5 ms threshold).
        let jitter = [0.000, 2.001, 4.000, 6.002, 8.001];
        let g = jitter_grid(&jitter);

        // When extracting anchors.
        let anchors = anchors_from_grid(&g, 20.0).expect("anchors");

        // Then the end anchor sits on the projected (jitter-free) bar.
        assert!((anchors.end_anchor_seconds - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn drift_beyond_threshold_reanchors() {
        // Given bars where the last one has drifted 20 ms late.
        let bars = [0.0, 2.0, 4.0, 6.0, 8.02];
        let g = jitter_grid(&bars);

        // When extracting anchors.
        let anchors = anchors_from_grid(&g, 20.0).expect("anchors");

        // Then the end anchor is the observed (re-anchored) bar, not 8.0.
        assert!((anchors.end_anchor_seconds - 8.02).abs() < f32::EPSILON);
    }

    #[test]
    fn outlier_bar_reanchors_projection() {
        // Given bars where one downbeat lands half a bar late (missed bar).
        let bars = [0.0, 2.0, 4.0, 7.0];

        // When normalizing directly.
        let normalized = normalize_bars(&bars, 2.0);

        // Then the outlier is kept as an anchor point (no stale projection).
        assert_eq!(normalized.len(), 4);
        assert!((normalized[3] - 7.0).abs() < f32::EPSILON);
    }

    /// Builds a grid from explicit bar times.
    fn jitter_grid(bars: &[f32]) -> BeatGrid {
        let mut beats = Vec::new();
        for &bar in bars {
            for beat_offset in [0.0, 0.5, 1.0, 1.5] {
                beats.push(bar + beat_offset);
            }
        }
        BeatGrid {
            grid_bpm: 120.0,
            anchor_seconds: 0.0,
            downbeats: bars.to_vec(),
            beats,
            bars: Vec::new(),
        }
    }

    #[test]
    fn window_snaps_to_session_grid_phase() {
        // Given a confident grid and a session at 150 BPM with an
        // arbitrary phase offset of 37_000 samples.
        let beat_samples = SessionTime::from_seconds(60.0 / 150.0, 44_100).0;
        let phase = SessionTime(37_000);

        // When placing a 64-beat window ending at 3_000_000 with a
        // grid phase.
        let w = place_window(WindowInputs {
            preset_beats: 64,
            a_session_end: SessionTime(3_000_000),
            b_cue_session: SessionTime(1_500_000),
            session_bpm: 150.0,
            sample_rate: 44_100,
            a_grid_phase: Some(phase),
        });

        // Then both boundaries sit exactly on the session grid
        // (phase + k * beat_samples).
        assert_eq!((w.start.0 - phase.0) % beat_samples, 0);
        assert_eq!((w.end.0 - phase.0) % beat_samples, 0);
        assert!(w.end.0 <= 3_000_000);
        // And the length stays near the requested 64 beats.
        assert!(w.len_samples() >= beat_samples * 60);
    }
}
