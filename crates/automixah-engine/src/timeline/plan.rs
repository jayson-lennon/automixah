//! Top-level session planning: playlist → [`SessionPlan`].
//!
//! Composes the pure pieces:
//!
//! 1. fold each track's BPM into [90, 180), then select the session
//!    tempo (median, user override wins);
//! 2. decide each track's stretch to the session tempo;
//! 3. place each adjacent pair's transition window on phrase
//!    boundaries (drift re-anchored grids, fallback when unconfident);
//! 4. emit segments with session-absolute sample times.

use crate::timeline::placement::{
    WindowInputs, anchors_from_grid, grid_is_confident, place_window,
};
use crate::timeline::stretch::decide_stretch;
use crate::timeline::tempo::select_target_bpm;
use crate::timeline::types::{
    CueKind, PresetName, Segment, SessionPlan, SessionTime, StretchDecision, TempoStrategy,
    TrackAnalysis, TrackHash, TransitionPlan, TransitionWindow,
};

/// Options for [`plan_session`].
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// User-selected target BPM (None = auto median).
    pub target_bpm: Option<f32>,
    /// Force pairwise drift-back on every segment ("deal with it"
    /// mode for outlier-heavy playlists). Default: constant
    /// session-BPM, with automatic DriftBack for out-of-band tracks.
    pub force_drift_back: bool,
    /// The active transition pair's window length in beats
    /// (default 64: the built-in 16-bar crossfade).
    pub transition_beats: usize,
    /// The active transition pair's name (surfaces in the plan).
    pub transition_name: String,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            target_bpm: None,
            force_drift_back: false,
            transition_beats: DEFAULT_PRESET_BEATS,
            transition_name: "LongCrossfade".into(),
        }
    }
}

/// Beats used by the default pair's window length (16 bars).
const DEFAULT_PRESET_BEATS: usize = 64;

/// Plans a continuous mix over the (user-ordered) playlist.
///
/// `user_bpm_override` selects the session tempo when provided;
/// otherwise the median of folded BPMs is used. Segments are laid out
/// sequentially: each transition overlaps the outgoing tail by its
/// window, so segment *starts* are `prior end − window length`.
#[must_use]
pub fn plan_session(tracks: &[TrackAnalysis], user_bpm_override: Option<f32>) -> SessionPlan {
    plan_with(
        tracks,
        PlanOptions {
            target_bpm: user_bpm_override,
            ..Default::default()
        },
    )
}

/// Plans with explicit [`PlanOptions`].
#[must_use]
pub fn plan_with(tracks: &[TrackAnalysis], options: PlanOptions) -> SessionPlan {
    let sample_rate = tracks.first().map_or(44_100, |t| t.sample_rate);
    let session_bpm = select_target_bpm(
        &tracks.iter().map(|t| t.bpm).collect::<Vec<_>>(),
        options.target_bpm,
    )
    .unwrap_or(120.0);

    let mut segments: Vec<Segment> = Vec::with_capacity(tracks.len());
    let mut cursor = SessionTime::ZERO;

    for (i, track) in tracks.iter().enumerate() {
        // The constant-grid BPM drives stretching (fold-idempotent;
        // it is the rounded, fitted grid tempo rather than the raw
        // estimate in `track.bpm`).
        let stretch = decide_stretch(
            grid_bpm_of(track),
            session_bpm,
            track.sample_rate,
            sample_rate,
        );
        let grid_confident = grid_is_confident(&track.beat_grid, track.grid_stability);

        // Segment length: full track stretched to session tempo.
        let stretched_len =
            SessionTime::from_seconds(track.duration * stretch.ratio.max(0.0), sample_rate);

        // Cue: where in the source track this segment starts. A user
        // in-cue (earliest source-valid, first track included) wins;
        // otherwise the first track starts at zero and incoming
        // tracks cue at a phrase anchor (a downbeat near the start)
        // when their grid is confident, else zero with a fallback.
        let (src_start, cue_warn) = match in_cue_for(track) {
            Some(cue) => (cue, None),
            None if i == 0 => (0_u64, None),
            None => cue_for(track),
        };
        if let Some(reason) = &cue_warn {
            eprintln!(
                "[plan] cue fallback for {} ({}): starting at 0",
                track.hash.0, reason
            );
        }

        // Audible span: full stretch minus the cue skipped into
        // the source (incoming tracks only).
        let audible_len = SessionTime(stretched_len.0.saturating_sub(src_stretched(
            track,
            src_start,
            session_bpm,
            sample_rate,
        )));

        // Overlap geometry: an incoming segment starts at the
        // *previous* transition's window start, so its intro plays
        // under the outgoing track's outro.
        let session_start = match segments.last() {
            Some(prev) => prev.transition.as_ref().map_or(cursor, |t| t.window.start),
            None => cursor,
        };

        let transition = next_transition(
            tracks,
            i,
            session_bpm,
            sample_rate,
            session_start,
            audible_len,
            grid_confident,
            options.transition_beats,
            &options.transition_name,
        );

        // Tempo strategy: constant session-BPM by default; DriftBack
        // when forced or when this incoming track is out of band
        // (it eases from the previous track's tempo back to its own).
        let use_drift_back = i > 0 && (options.force_drift_back || stretch.out_of_comfort_band);
        let stretch = if use_drift_back {
            drift_back_decision(track, &tracks[i - 1], session_bpm, sample_rate, stretch)
        } else {
            stretch
        };

        /// Converts a constant-ratio decision into DriftBack: during the
        /// overlap the incoming track plays at the *previous* track's
        /// (stretched) tempo, easing back to its own native ratio over
        /// `ease_bars` bars after the window.
        fn drift_back_decision(
            track: &TrackAnalysis,
            prev: &TrackAnalysis,
            session_bpm: f32,
            sample_rate: u32,
            constant: StretchDecision,
        ) -> StretchDecision {
            let prev_ratio = decide_stretch(
                grid_bpm_of(prev),
                session_bpm,
                prev.sample_rate,
                sample_rate,
            )
            .ratio;
            let native = decide_stretch(
                grid_bpm_of(track),
                grid_bpm_of(track),
                track.sample_rate,
                sample_rate,
            )
            .ratio;
            StretchDecision {
                strategy: TempoStrategy::DriftBack {
                    overlap_ratio: prev_ratio,
                    native_ratio: native,
                    ease_bars: 8,
                },
                ..constant
            }
        }

        // Length: the audible span is the full stretched track minus
        // the cue we skipped into it.
        let len_samples = stretched_len.0.saturating_sub(src_stretched(
            track,
            src_start,
            session_bpm,
            sample_rate,
        ));

        segments.push(Segment {
            track_hash: TrackHash(track.hash.0.clone()),
            src_start,
            session_start,
            len_samples,
            stretch,
            transition: transition.map(|(window, preset)| TransitionPlan { window, preset }),
        });

        // Cue-on-grid verification: the incoming cue must coincide
        // with the outgoing window's session-grid phase (within ~2 ms
        // on the resample path; WSOLA adds ±10 ms out of band).
        if i > 0 {
            verify_cue_alignment(&segments[i - 1], &segments[i], session_bpm, sample_rate);
        }

        cursor = SessionTime(session_start.0 + stretched_len.0);
    }

    SessionPlan {
        session_bpm,
        sample_rate,
        segments,
    }
}

/// The constant-grid tempo driving stretch decisions: the fitted
/// `grid_bpm` when present, else the raw BPM estimate (fallback
/// path — grids are empty/unconfident there).
fn grid_bpm_of(track: &TrackAnalysis) -> f32 {
    let grid_bpm = track.beat_grid.grid_bpm;
    if grid_bpm.is_finite() && grid_bpm > 0.0 {
        grid_bpm
    } else {
        track.bpm
    }
}

/// Warns when an incoming segment's cue does not land on the
/// outgoing transition window's session-grid phase.
///
/// With a constant grid both decks share the session beat period,
/// so a grid-derived cue lands on the grid by construction; drift
/// beyond ~2 ms means a fallback cue (no grid); > 10 ms indicates
/// WSOLA jitter or a mis-anchored grid.
fn verify_cue_alignment(
    outgoing: &Segment,
    incoming: &Segment,
    session_bpm: f32,
    sample_rate: u32,
) {
    let Some(window) = outgoing.transition.as_ref().map(|t| &t.window) else {
        return;
    };
    let beat_samples = SessionTime::from_seconds(60.0 / session_bpm, sample_rate).0;
    let phase = window.start.0 % beat_samples;
    let cue_phase = incoming.session_start.0 % beat_samples;
    let raw = cue_phase.abs_diff(phase);
    let drift = raw.min(beat_samples - raw);
    #[expect(clippy::cast_precision_loss, reason = "sample counts are small")]
    let ms = (drift as f64) * 1000.0 / f64::from(sample_rate);
    if ms > 2.0 {
        eprintln!(
            "[plan] cue off session grid by {ms:.1} ms for {} (resample tolerance 2 ms, WSOLA 10 ms)",
            incoming.track_hash.0
        );
    }
}

/// Session-time length of a source-frame cue position under the
/// track's session stretch (tempo-match × rate conversion).
fn src_stretched(track: &TrackAnalysis, src_start: u64, session_bpm: f32, sample_rate: u32) -> u64 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "cue samples fit u64"
    )]
    let stretched = (src_start as f64
        * f64::from(
            decide_stretch(
                grid_bpm_of(track),
                session_bpm,
                track.sample_rate,
                sample_rate,
            )
            .ratio,
        )) as u64;
    stretched
}

/// Picks the source cue for an incoming track: its first downbeat
/// (the natural start of the music, after any silence/intro) when
/// the grid is confident, else zero with a reason string for the
/// caller to warn about.
fn cue_for(track: &TrackAnalysis) -> (u64, Option<&'static str>) {
    if !grid_is_confident(&track.beat_grid, track.grid_stability) {
        return (0, Some("beat grid not confident"));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "downbeat samples fit u64"
    )]
    let first =
        (f64::from(track.beat_grid.downbeats[0]) * f64::from(track.sample_rate)).round() as u64;
    (first, None)
}

/// The track's source-cue offset in seconds: the first downbeat
/// for confident grids, else zero (the fallback cue).
fn cue_seconds_of(track: &TrackAnalysis) -> f32 {
    if grid_is_confident(&track.beat_grid, track.grid_stability) {
        track.beat_grid.downbeats[0]
    } else {
        0.0
    }
}

/// The earliest source-valid user in-cue, if any. Positions at or
/// before the track's end are considered valid; the planner consumes
/// the value verbatim (no plan-time snapping — the UI snaps on
/// write). `None` means no usable user in-cue, so the caller falls
/// back to the grid-derived cue.
fn in_cue_for(track: &TrackAnalysis) -> Option<u64> {
    track
        .cues
        .earliest_valid(CueKind::In, source_duration_frames(track))
}

/// The effective source-frame in-cue. A grid-derived fallback is converted
/// to source frames once so out-cue mapping can remain frame-exact.
fn effective_cue_frames(track: &TrackAnalysis) -> u64 {
    in_cue_for(track).unwrap_or_else(|| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "downbeat samples fit u64"
        )]
        {
            (f64::from(cue_seconds_of(track)) * f64::from(track.sample_rate)).round() as u64
        }
    })
}

/// Decoded source duration expressed as the final valid source frame.
fn source_duration_frames(track: &TrackAnalysis) -> u64 {
    #[expect(clippy::cast_precision_loss, reason = "track duration to frames")]
    let frames = (track.duration * track.sample_rate as f32).ceil();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "decoded track duration fits u64"
    )]
    let frames = frames.max(0.0) as u64;
    frames
}

/// The track's effective cue in source seconds: the user in-cue when
/// present, else the grid's first downbeat (the fallback cue).
fn effective_cue_seconds(track: &TrackAnalysis) -> f32 {
    match in_cue_for(track) {
        Some(frames) => {
            #[expect(clippy::cast_precision_loss, reason = "cue frames to seconds")]
            let seconds = frames as f32 / track.sample_rate as f32;
            seconds
        }
        None => cue_seconds_of(track),
    }
}

struct OutWindowInputs {
    cue_frames: u64,
    ratio: f32,
    cursor: SessionTime,
    audio_end: SessionTime,
    session_bpm: f32,
    sample_rate: u32,
    transition_beats: usize,
}

/// Places the transition window's START at A's earliest source-valid
/// user out-cue, spanning the full automation length forward.
///
/// Returns `None` when no out-cue is usable: every candidate is at or before
/// the selected in-cue, or lacks enough tail to fit the whole window. The
/// caller then falls back to grid-derived placement. The window is consumed
/// verbatim (no plan-time snapping): source-grid snapping at write time
/// already aligns it with the session grid once stretched.
fn out_window_for(a: &TrackAnalysis, inputs: OutWindowInputs) -> Option<TransitionWindow> {
    let duration_frames = source_duration_frames(a);
    #[expect(clippy::cast_precision_loss, reason = "window length is small")]
    let window_len = SessionTime::from_seconds(
        inputs.transition_beats.max(1) as f32 * (60.0 / inputs.session_bpm),
        inputs.sample_rate,
    );

    a.cues
        .valid_positions(CueKind::Out, duration_frames)
        .filter_map(|(_, out_frames)| {
            let source_delta = out_frames
                .checked_sub(inputs.cue_frames)
                .filter(|&delta| delta > 0)?;
            #[expect(
                clippy::cast_precision_loss,
                reason = "source frame delta fits the audio timeline"
            )]
            let offset = (source_delta as f64 * f64::from(inputs.ratio)).round();
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "mapped source offset fits session samples"
            )]
            let offset = offset as u64;
            let start = SessionTime(inputs.cursor.0.checked_add(offset)?);
            let end = SessionTime(start.0.checked_add(window_len.0)?);
            (end <= inputs.audio_end).then_some((out_frames, TransitionWindow { start, end }))
        })
        .min_by_key(|(out_frames, _)| *out_frames)
        .map(|(_, window)| window)
}

/// Places the transition out of track `i` (into `i + 1`), if any.
///
/// The window closes at A's natural end: the stretched position of
/// its last downbeat with audio when the grid is confident, else its
/// full stretched end. B's cue maps onto the window start via
/// `src_start`.
#[allow(clippy::too_many_arguments)]
fn next_transition(
    tracks: &[TrackAnalysis],
    i: usize,
    session_bpm: f32,
    sample_rate: u32,
    cursor: SessionTime,
    stretched_len: SessionTime,
    grid_confident: bool,
    transition_beats: usize,
    transition_name: &str,
) -> Option<(TransitionWindow, PresetName)> {
    let next = tracks.get(i + 1)?;
    let a = &tracks[i];

    let a_anchor = if grid_confident {
        anchors_from_grid(&a.beat_grid, a.duration)
    } else {
        None
    };
    let _b_anchor = anchors_from_grid(&next.beat_grid, next.duration);

    let cue = effective_cue_seconds(a);
    let cue_frames = effective_cue_frames(a);
    let ratio = decide_stretch(grid_bpm_of(a), session_bpm, a.sample_rate, sample_rate).ratio;

    // Window closes at A's natural end in session time: the
    // stretched position of its last downbeat with audio when the
    // grid is confident, else its full stretched end.
    let full_end = SessionTime(cursor.0 + stretched_len.0);
    let a_session_end = a_anchor.as_ref().map_or(full_end, |anchor| {
        SessionTime(cursor.0.saturating_add(
            SessionTime::from_seconds((anchor.end_anchor_seconds - cue) * ratio, sample_rate).0,
        ))
    });

    // A valid user out-cue anchors the window START (B begins mixing
    // in there and the full automation spans [out, out + len]). It
    // replaces the grid-derived fallback placement entirely;
    // otherwise the fallback window (closing at A's end) is used.
    let window = out_window_for(
        a,
        OutWindowInputs {
            cue_frames,
            ratio,
            cursor,
            audio_end: full_end,
            session_bpm,
            sample_rate,
            transition_beats,
        },
    )
    .unwrap_or_else(|| {
        place_window(WindowInputs {
            preset_beats: transition_beats.max(1),
            a_session_end,
            b_cue_session: SessionTime::ZERO,
            session_bpm,
            sample_rate,
            a_grid_phase: if grid_confident {
                a_grid_phase(a, session_bpm, sample_rate, cursor, stretched_len)
            } else {
                None
            },
        })
    });

    Some((window, PresetName(transition_name.to_owned())))
}

/// A's stretched-grid phase in session samples, when its grid is
/// confident: the session position of A's first grid beat at or
/// after the source cue, computed from the grid anchor and the
/// stretch ratio, then reduced onto the session beat grid.
///
/// A source beat at `t` lands at session time
/// `session_start + (t - cue_seconds) * ratio`, and because every
/// deck stretches by `grid_bpm/session_bpm`, the stretched beat
/// period is exactly `60/session_bpm` — so this one phase pins
/// A's whole beat pattern onto the shared session grid.
fn a_grid_phase(
    a: &TrackAnalysis,
    session_bpm: f32,
    sample_rate: u32,
    session_start: SessionTime,
    stretched_len: SessionTime,
) -> Option<SessionTime> {
    let grid_bpm = a.beat_grid.grid_bpm;
    if !grid_bpm.is_finite() || grid_bpm <= 0.0 {
        return None;
    }
    let cue_seconds = f64::from(session_start.as_seconds(sample_rate));
    let ratio =
        f64::from(decide_stretch(grid_bpm_of(a), session_bpm, a.sample_rate, sample_rate).ratio);
    // First grid beat at or after time zero in source time: beats
    // sit at anchor + n * beat_len, so n = ceil(-anchor / beat_len).
    let beat_len = 60.0 / f64::from(grid_bpm);
    let anchor = f64::from(a.beat_grid.anchor_seconds);
    let n = (-anchor / beat_len).ceil();
    let first_beat = anchor + n * beat_len;
    let session_beat = 60.0 / f64::from(session_bpm);
    let offset = ((first_beat - cue_seconds) * ratio).rem_euclid(session_beat);
    let phase_samples = session_start
        .0
        .saturating_add(SessionTime::from_seconds(offset as f32, sample_rate).0);
    // Guard: the phase must lie within A's audible session span.
    let span_end = session_start.0.saturating_add(stretched_len.0);
    if phase_samples > span_end {
        return None;
    }
    Some(SessionTime(phase_samples))
}

/// Builds a synthetic [`TrackAnalysis`] for tests.
#[cfg(test)]
pub(crate) fn synthetic_track(hash: &str, bpm: f32, duration: f32, beats: usize) -> TrackAnalysis {
    let beat_len = 60.0 / bpm;
    #[expect(clippy::cast_precision_loss, reason = "test helper: small indices")]
    let downbeats: Vec<f32> = (0..=beats / 4).map(|i| i as f32 * beat_len * 4.0).collect();
    #[expect(clippy::cast_precision_loss, reason = "test helper: small indices")]
    let beat_count: Vec<f32> = (0..beats).map(|i| i as f32 * beat_len).collect();
    TrackAnalysis {
        hash: TrackHash(hash.into()),
        bpm,
        bpm_confidence: 0.9,
        key: djcore::Key {
            root: 0,
            mode: djcore::KeyMode::Major,
        },
        duration,
        beat_grid: BeatGrid {
            grid_bpm: bpm,
            anchor_seconds: 0.0,
            downbeats,
            beats: beat_count,
            bars: Vec::new(),
        },
        grid_stability: 0.9,
        sample_rate: 44_100,
        channels: 2,
        format: "wav".into(),
        cues: TestCuePoints::default(),
    }
}

#[cfg(test)]
use djcore::analyzer::BeatGrid;

// `CuePoints` becomes a lib import once the planner consumes cues (Phase 3).
#[cfg(test)]
use crate::timeline::types::CuePoints as TestCuePoints;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_back_used_for_out_of_band_incoming_track() {
        // Given 120 → 150 with a 120 session target.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 150.0, 60.0, 240),
        ];

        // When planning at 120.
        let plan = plan_session(&tracks, Some(120.0));

        // Then segment 1 uses DriftBack (150 is 25% off = out of band).
        assert!(matches!(
            plan.segments[1].stretch.strategy,
            TempoStrategy::DriftBack { .. }
        ));
    }

    #[test]
    fn in_band_incoming_track_stays_session_bpm() {
        // Given 120 → 124 at target 120.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 124.0, 60.0, 240),
        ];

        // When planning.
        let plan = plan_session(&tracks, Some(120.0));

        // Then segment 1 stays constant-ratio.
        assert_eq!(plan.segments[1].stretch.strategy, TempoStrategy::SessionBpm);
    }

    #[test]
    fn force_drift_back_applies_to_all_but_first() {
        // Given two in-band tracks.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 122.0, 60.0, 240),
        ];

        // When planning with force_drift_back.
        let plan = plan_with(
            &tracks,
            PlanOptions {
                target_bpm: Some(120.0),
                force_drift_back: true,
                ..Default::default()
            },
        );

        // Then segment 0 is constant and segment 1 is DriftBack.
        assert_eq!(plan.segments[0].stretch.strategy, TempoStrategy::SessionBpm);
        assert!(matches!(
            plan.segments[1].stretch.strategy,
            TempoStrategy::DriftBack { .. }
        ));
    }

    #[test]
    fn drift_back_ratio_eases_from_overlap_to_native() {
        // Given a DriftBack strategy.
        let s = TempoStrategy::DriftBack {
            overlap_ratio: 1.0,
            native_ratio: 1.25,
            ease_bars: 8,
        };
        // 120 BPM → bar = 2 s; overlap at [0, 16) s of segment time.
        const BPM: f32 = 120.0;
        const OV_START: f32 = 0.0;
        const OV_LEN: f32 = 16.0;

        // Then before/inside the overlap the ratio is overlap_ratio…
        assert!((s.ratio_at(0.0, 5.0, OV_START, OV_LEN, BPM) - 1.0).abs() < f32::EPSILON);
        assert!((s.ratio_at(0.0, 15.9, OV_START, OV_LEN, BPM) - 1.0).abs() < 1e-6);
        // …halfway through the 16 s ease it is halfway in ratio…
        let mid = s.ratio_at(0.0, OV_LEN + 8.0, OV_START, OV_LEN, BPM);
        assert!((mid - 1.125).abs() < 1e-3);
        // …and after the full ease it reaches native.
        let end = s.ratio_at(0.0, OV_LEN + 16.1, OV_START, OV_LEN, BPM);
        assert!((end - 1.25).abs() < 1e-6);
    }

    #[test]
    fn plan_uses_median_target_bpm_by_default() {
        // Given a mixed-BPM playlist.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 126.0, 60.0, 240),
            synthetic_track("c", 130.0, 60.0, 240),
        ];

        // When planning without an override.
        let plan = plan_session(&tracks, None);

        // Then the session tempo is the folded median (126).
        assert!((plan.session_bpm - 126.0).abs() < f32::EPSILON);
    }

    #[test]
    fn plan_honors_user_bpm_override() {
        // Given the same playlist with a user override.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 126.0, 60.0, 240),
            synthetic_track("c", 130.0, 60.0, 240),
        ];

        // When planning with an override of 124.
        let plan = plan_session(&tracks, Some(124.0));

        // Then the session tempo is 124.
        assert!((plan.session_bpm - 124.0).abs() < f32::EPSILON);
    }

    #[test]
    fn plan_segments_are_sequential_and_overlapping() {
        // Given two tracks.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 122.0, 60.0, 240),
        ];

        // When planning.
        let plan = plan_session(&tracks, None);

        // Then segment 0 has a transition whose window precedes segment 1.
        let t = plan.segments[0].transition.as_ref().expect("transition");
        assert!(t.window.end.0 <= plan.segments[1].session_start.0 + t.window.len_samples());
        assert!(t.window.start.0 < t.window.end.0);
    }

    #[test]
    fn plan_stretches_each_track_to_session_tempo() {
        // Given tracks at 120 and 150 with a 120 target.
        let tracks = vec![
            synthetic_track("a", 120.0, 60.0, 240),
            synthetic_track("b", 150.0, 60.0, 240),
        ];

        // When planning at 120.
        let plan = plan_session(&tracks, Some(120.0));

        // Then track b is stretched by 150/120 (duration ×1.25) and is out of band.
        assert!((plan.segments[1].stretch.ratio - 150.0 / 120.0).abs() < 1e-6);
        assert!(plan.segments[1].stretch.out_of_comfort_band);
    }

    #[test]
    fn full_plan_snapshot_on_mixed_bpm_playlist() {
        // Given a mixed-BPM playlist spanning fold boundaries.
        let tracks = vec![
            synthetic_track("a", 148.0, 344.0, 848),
            synthetic_track("b", 100.0, 180.0, 300),
            synthetic_track("c", 175.0, 200.0, 583),
            synthetic_track("d", 96.0, 150.0, 240),
        ];

        // When planning with no override (median of folded = (148,100,175,96) → 124).
        let plan = plan_session(&tracks, None);

        // Then the snapshot holds: bpm, segment count, starts, stretch modes.
        let snapshot = format!(
            "bpm={:.1} segments={} first_len={} second_start={} cut_preset={}",
            plan.session_bpm,
            plan.segments.len(),
            plan.segments[0].len_samples,
            plan.segments[1].session_start.0,
            plan.segments[0]
                .transition
                .as_ref()
                .map_or("", |t| &t.preset.0),
        );
        assert_eq!(
            snapshot,
            "bpm=124.0 segments=4 first_len=18106608 second_start=16644193 cut_preset=LongCrossfade"
        );
    }

    #[test]
    fn first_track_in_cue_is_honored() {
        // Given a first track with a user in-cue at 8 s.
        let mut a = synthetic_track("a", 120.0, 200.0, 800);
        a.cues = TestCuePoints::with_in(0, 44_100_u64 * 8);
        let b = synthetic_track("b", 120.0, 200.0, 800);
        let tracks = vec![a, b];

        // When planning.
        let plan = plan_session(&tracks, Some(120.0));

        // Then segment 0 starts at the user in-cue, not at frame 0.
        assert_eq!(plan.segments[0].src_start, 44_100_u64 * 8);
    }

    #[test]
    fn invalid_in_cue_falls_back_to_each_track_grid_policy() {
        // Given an out-of-range in-cue on the first track and no user cue on
        // a confident-grid incoming track.
        let mut first = synthetic_track("first", 120.0, 20.0, 80);
        first.cues = TestCuePoints::with_in(0, 44_100_u64 * 25);
        let second = synthetic_track("second", 120.0, 20.0, 80);

        // When planning the playlist.
        let plan = plan_session(&[first, second], Some(120.0));

        // Then each segment uses its existing independent fallback.
        assert_eq!(plan.segments[0].src_start, 0, "first-track fallback");
        assert_eq!(
            plan.segments[1].src_start, 0,
            "the confident grid starts at its first downbeat"
        );
    }

    #[test]
    fn planner_consumes_persisted_in_frame_without_grid_snapping() {
        // Given an incoming user cue deliberately placed between grid beats,
        // followed by a manual grid adjustment.
        let first = synthetic_track("first", 120.0, 200.0, 800);
        let mut second = synthetic_track("second", 120.0, 200.0, 800);
        let persisted_frame = 12_345_u64;
        second.cues = TestCuePoints::with_in(1, persisted_frame);
        second.beat_grid.anchor_seconds = 0.37;
        second.beat_grid.downbeats = (0..50).map(|i| 0.37 + i as f32 * 2.0).collect();

        // When planning after that grid adjustment.
        let plan = plan_session(&[first, second], Some(120.0));

        // Then the planner uses the stored frame exactly rather than
        // snapping it to the changed grid.
        assert_eq!(plan.segments[1].src_start, persisted_frame);
    }

    #[test]
    fn earliest_in_cue_wins_over_slot_order() {
        // Given an in-cue in slot 3 (later) and one in slot 0 placed earlier.
        let a = synthetic_track("a", 120.0, 200.0, 800);
        let mut b = synthetic_track("b", 120.0, 200.0, 800);
        b.cues = TestCuePoints::with_in(0, 44_100_u64 * 40);
        b.cues.set(CueKind::In, 3, 44_100_u64 * 12);
        let tracks = vec![a, b];

        // When planning.
        let plan = plan_session(&tracks, Some(120.0));

        // Then the earliest position (slot 3 at 12 s) is selected — slots are labels.
        assert_eq!(plan.segments[1].src_start, 44_100_u64 * 12);
    }

    #[test]
    fn out_cue_anchors_transition_window_start() {
        // Given an outgoing track with a valid out-cue at 90 s and tail for the window.
        let mut a = synthetic_track("a", 120.0, 200.0, 800);
        a.cues = TestCuePoints::with_out(0, 44_100_u64 * 90);
        let b = synthetic_track("b", 120.0, 200.0, 800);
        let tracks = vec![a, b];

        // When planning.
        let plan = plan_session(&tracks, Some(120.0));

        // Then the window starts exactly at the out-cue (consumed verbatim, no snap).
        let t = plan.segments[0].transition.as_ref().expect("transition");
        let expected = (90.0_f64 * 44_100.0_f64).round() as u64;
        assert_eq!(t.window.start.0, expected);
    }

    #[test]
    fn out_cue_too_late_falls_back_to_grid_placement() {
        // Given an out-cue so late the full window cannot fit before A's end.
        let mut a = synthetic_track("a", 120.0, 100.0, 400); // 100 s track
        a.cues = TestCuePoints::with_out(0, 44_100_u64 * 95); // 95 s out
        let b = synthetic_track("b", 120.0, 200.0, 800);
        let tracks = vec![a, b];

        // When planning.
        let plan = plan_session(&tracks, Some(120.0));

        // Then the out-cue is ignored and the grid-derived fallback closes A's window.
        let t = plan.segments[0].transition.as_ref().expect("transition");
        assert_ne!(t.window.start.0, 44_100_u64 * 95);
    }

    #[test]
    fn out_cue_maps_source_frame_delta_with_stretch_ratio() {
        // Given an outgoing track stretched from 100 BPM to a 120 BPM
        // session, with source-frame in/out cues.
        let mut a = synthetic_track("a", 100.0, 200.0, 800);
        let in_frame = 12_345_u64;
        let out_frame = 234_567_u64;
        a.cues = TestCuePoints {
            ins: [Some(in_frame), None, None, None],
            outs: [Some(out_frame), None, None, None],
        };
        let b = synthetic_track("b", 120.0, 200.0, 800);

        // When planning a four-beat transition.
        let plan = plan_with(
            &[a, b],
            PlanOptions {
                target_bpm: Some(120.0),
                transition_beats: 4,
                ..Default::default()
            },
        );

        // Then the window begins at the exact source-frame delta mapped by
        // the stretch ratio, with no grid-phase snapping.
        let ratio = 100.0_f64 / 120.0_f64;
        let expected_offset = (((out_frame - in_frame) as f64) * ratio).round() as u64;
        let window = &plan.segments[0]
            .transition
            .as_ref()
            .expect("transition")
            .window;
        assert_eq!(window.start.0, expected_offset);
        assert_eq!(
            window.end.0 - window.start.0,
            SessionTime::from_seconds(2.0, 44_100).0
        );
    }

    #[test]
    fn equal_out_and_in_cues_use_fallback_placement() {
        // Given an outgoing track whose out-cue is exactly its selected in-cue.
        let mut a = synthetic_track("a", 120.0, 200.0, 800);
        let cue = 44_100_u64 * 50;
        a.cues = TestCuePoints {
            ins: [Some(cue), None, None, None],
            outs: [Some(cue), None, None, None],
        };
        let b = synthetic_track("b", 120.0, 200.0, 800);

        // When planning the transition.
        let plan = plan_session(&[a, b], Some(120.0));

        // Then the in-cue remains active but the zero-length out cue is ignored.
        assert_eq!(plan.segments[0].src_start, cue);
        let window = &plan.segments[0]
            .transition
            .as_ref()
            .expect("transition")
            .window;
        assert_ne!(window.start.0, 0);
    }

    #[test]
    fn out_cues_all_before_in_cue_use_fallback_placement() {
        // Given an outgoing track whose only out-cue is before its in-cue.
        let mut a = synthetic_track("a", 120.0, 200.0, 800);
        a.cues = TestCuePoints::with_in(0, 44_100_u64 * 50);
        a.cues.set(CueKind::Out, 0, 44_100_u64 * 40);
        let b = synthetic_track("b", 120.0, 200.0, 800);

        // When planning the transition.
        let plan = plan_session(&[a, b], Some(120.0));

        // Then the in-cue is honored but the out side independently falls
        // back to grid-derived placement.
        assert_eq!(plan.segments[0].src_start, 44_100_u64 * 50);
        let window = &plan.segments[0]
            .transition
            .as_ref()
            .expect("transition")
            .window;
        assert_ne!(window.start.0, 44_100_u64 * 40);
    }
    #[test]
    fn later_out_cue_wins_when_earlier_out_cue_is_before_in_cue() {
        // Given an outgoing track with an earlier invalid out-cue and a later
        // out-cue that is after the selected in-cue and has enough tail.
        let mut a = synthetic_track("a", 120.0, 200.0, 800);
        let in_frame = 44_100_u64 * 50;
        let valid_out = 44_100_u64 * 90;
        a.cues = TestCuePoints {
            ins: [Some(in_frame), None, None, None],
            outs: [Some(44_100_u64 * 40), None, None, Some(valid_out)],
        };
        let b = synthetic_track("b", 120.0, 200.0, 800);

        // When planning the transition.
        let plan = plan_session(&[a, b], Some(120.0));

        // Then the later source-valid out-cue is selected rather than
        // discarding the entire out-cue set.
        let window = &plan.segments[0]
            .transition
            .as_ref()
            .expect("transition")
            .window;
        assert_eq!(window.start.0, valid_out - in_frame);
    }
}
