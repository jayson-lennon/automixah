//! Timeline planning types: analyses, session plans, segments, and
//! transitions.
//!
//! All session times are [`SessionTime`] — sample counts at the engine
//! sample rate — so transition placement is sample-exact by construction.

use serde::{Deserialize, Serialize};

use djcore::analyzer::BeatGrid;
use djcore::key::Key;

/// A point in session time, in samples at the engine sample rate.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct SessionTime(pub u64);

impl SessionTime {
    /// The session start.
    pub const ZERO: Self = Self(0);

    /// Converts a duration in seconds to samples at `sample_rate`.
    #[must_use]
    pub fn from_seconds(seconds: f32, sample_rate: u32) -> Self {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        Self((seconds.max(0.0) * sample_rate as f32).round() as u64)
    }
    /// Converts to seconds at `sample_rate`.
    #[must_use]
    pub fn as_seconds(self, sample_rate: u32) -> f32 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "sample counts fit f32 mantissa in practice"
        )]
        {
            self.0 as f32 / sample_rate as f32
        }
    }
}

/// Content-hash identity of a track in the library.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackHash(pub String);

impl std::fmt::Display for TrackHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Number of cue slots per kind.
pub const CUE_SLOTS: usize = 4;

/// Which kind of cue a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CueKind {
    /// Where the track begins (the segment's source start).
    In,
    /// Where the transition automation begins (the window start).
    Out,
}

/// User-authored cue points for one track, as source-frame positions.
///
/// Slots are identifiers only — the UI numbers them 1..=4 — and never
/// determine precedence. The renderer selects the earliest valid position
/// of each kind; a slot number can therefore win even when a lower-numbered
/// slot holds a later position.
///
/// Positions are source frames at the track's own sample rate; they are
/// snapped to the nearest grid beat in the UI before persistence and
/// consumed verbatim by the planner (no plan-time snapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CuePoints {
    /// In-cue source frames per slot (`None` = unset).
    pub ins: [Option<u64>; CUE_SLOTS],
    /// Out-cue source frames per slot (`None` = unset).
    pub outs: [Option<u64>; CUE_SLOTS],
}

impl CuePoints {
    /// The position in `kind` slot `slot`, if set.
    #[must_use]
    pub fn get(&self, kind: CueKind, slot: usize) -> Option<u64> {
        self.array(kind).get(slot).copied().flatten()
    }

    /// Sets `kind` slot `slot` to `frames`. Out-of-range slots are ignored.
    pub fn set(&mut self, kind: CueKind, slot: usize, frames: u64) {
        if let Some(entry) = self.array_mut(kind).get_mut(slot) {
            *entry = Some(frames);
        }
    }

    /// Clears `kind` slot `slot`. Out-of-range slots are ignored.
    pub fn delete(&mut self, kind: CueKind, slot: usize) {
        if let Some(entry) = self.array_mut(kind).get_mut(slot) {
            *entry = None;
        }
    }

    /// The earliest set position of `kind` at or before `duration_frames`.
    ///
    /// Positions past the track's end are ignored. Relationship checks such
    /// as out-after-in and transition-tail availability belong to the planner.
    #[must_use]
    pub fn earliest_valid(&self, kind: CueKind, duration_frames: u64) -> Option<u64> {
        self.array(kind)
            .iter()
            .flatten()
            .copied()
            .filter(|&frames| frames <= duration_frames)
            .min()
    }

    /// Whether any slot of `kind` is set.
    #[must_use]
    pub fn has_any(&self, kind: CueKind) -> bool {
        self.array(kind).iter().any(Option::is_some)
    }

    /// Whether no slot of either kind is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.has_any(CueKind::In) && !self.has_any(CueKind::Out)
    }

    fn array(&self, kind: CueKind) -> &[Option<u64>; CUE_SLOTS] {
        match kind {
            CueKind::In => &self.ins,
            CueKind::Out => &self.outs,
        }
    }

    fn array_mut(&mut self, kind: CueKind) -> &mut [Option<u64>; CUE_SLOTS] {
        match kind {
            CueKind::In => &mut self.ins,
            CueKind::Out => &mut self.outs,
        }
    }
}

/// The persisted analysis of one track (the OPFS `/analysis/<hash>.json`
/// shape). Wraps the djcore analyzer output plus library metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackAnalysis {
    /// Content hash of the source file.
    pub hash: TrackHash,
    /// Octave-unnormalized BPM as detected.
    pub bpm: f32,
    /// BPM confidence in `[0, 1]`.
    pub bpm_confidence: f32,
    /// Detected musical key.
    pub key: Key,
    /// Duration in seconds.
    pub duration: f32,
    /// Beat grid (downbeats, beats, bars in seconds).
    pub beat_grid: BeatGrid,
    /// Beat-grid stability in `[0, 1]`.
    pub grid_stability: f32,
    /// Source sample rate in Hz.
    pub sample_rate: u32,
    /// Source channel count.
    pub channels: u16,
    /// Container/codec extension (e.g. `"mp3"`).
    pub format: String,
}

/// How a track's tempo is matched to the session BPM.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StretchMode {
    /// Pitch-adjusted resampling (turntable-style). Cheap, sample-exact;
    /// used within the ±8% comfort band.
    Resample,
    /// Pitch-preserving WSOLA time-stretch. Used beyond ±8%.
    Wsola,
}

impl StretchMode {
    /// Decides the mode from a stretch ratio.
    ///
    /// `|ratio - 1| <= 0.08` → [`StretchMode::Resample`], else
    /// [`StretchMode::Wsola`].
    #[must_use]
    pub fn for_ratio(ratio: f32) -> Self {
        if (ratio - 1.0).abs() <= 0.08 + f32::EPSILON {
            Self::Resample
        } else {
            Self::Wsola
        }
    }
}

/// Tempo-matching decision for one segment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StretchDecision {
    /// Selected stretch mode.
    pub mode: StretchMode,
    /// Output rate / input rate (includes engine-rate conversion).
    pub ratio: f32,
    /// Whether `|ratio - 1|` exceeds the ±8% comfort band (UI tints).
    pub out_of_comfort_band: bool,
    /// Tempo strategy for this segment.
    pub strategy: TempoStrategy,
}

/// How a track's tempo maps onto the session over time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum TempoStrategy {
    /// Constant stretch to the session BPM (default; "fixed-BPM"
    /// sessions where every track plays at the target tempo).
    #[default]
    SessionBpm,
    /// Pairwise drift-back: the incoming track matches the outgoing
    /// track's tempo during the transition overlap, then eases back
    /// to its native tempo over the bars following the window. Used
    /// for outlier tracks that would otherwise be stretched hard.
    DriftBack {
        /// Ratio during the overlap window (matches outgoing tempo).
        overlap_ratio: f32,
        /// Native ratio (target tempo → track's own tempo), reached
        /// after `ease_bars` bars past the window.
        native_ratio: f32,
        /// Bars over which the ratio eases from overlap to native.
        ease_bars: u32,
    },
}

impl StretchDecision {
    /// Builds a constant-ratio decision (SessionBpm strategy).
    #[must_use]
    pub fn constant(mode: StretchMode, ratio: f32, out_of_comfort_band: bool) -> Self {
        Self {
            mode,
            ratio,
            out_of_comfort_band,
            strategy: TempoStrategy::SessionBpm,
        }
    }
}

impl TempoStrategy {
    /// The ratio in effect at `seconds_into_segment` (segment time,
    /// starting at the segment's session start).
    ///
    /// `overlap_start`/`overlap_len` locate the transition window in
    /// segment time; they are ignored for [`TempoStrategy::SessionBpm`].
    #[must_use]
    pub fn ratio_at(
        &self,
        constant: f32,
        seconds_into_segment: f32,
        overlap_start: f32,
        overlap_len: f32,
        session_bpm: f32,
    ) -> f32 {
        match *self {
            TempoStrategy::SessionBpm => constant,
            TempoStrategy::DriftBack {
                overlap_ratio,
                native_ratio,
                ease_bars,
            } => {
                if seconds_into_segment < overlap_start {
                    return overlap_ratio;
                }
                let into_overlap = seconds_into_segment - overlap_start;
                if into_overlap <= overlap_len {
                    return overlap_ratio;
                }
                let ease_seconds = ease_bars as f32 * (240.0 / session_bpm);
                let eased = (into_overlap - overlap_len) / ease_seconds.max(f32::EPSILON);
                let t = eased.clamp(0.0, 1.0);
                // Cosine ease from overlap ratio to native ratio.
                let eased_t = 0.5 - 0.5 * (std::f32::consts::PI * t).cos();
                overlap_ratio + (native_ratio - overlap_ratio) * eased_t
            }
        }
    }
}

/// The phrase-aligned window in session time where two tracks overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionWindow {
    /// Session sample where the incoming track starts (B's cue point).
    pub start: SessionTime,
    /// Session sample where the outgoing track ends (window close).
    pub end: SessionTime,
}

impl TransitionWindow {
    /// Window length in samples.
    #[must_use]
    pub fn len_samples(self) -> u64 {
        self.end.0.saturating_sub(self.start.0)
    }

    /// Whether the window is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Name of a mixing automation preset driving a transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetName(pub String);

impl std::fmt::Display for PresetName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One planned transition between adjacent segments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionPlan {
    /// The overlap window in session time.
    pub window: TransitionWindow,
    /// Preset driving the automation curves.
    pub preset: PresetName,
}

/// One track's span in the session timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// Identity of the track being played.
    pub track_hash: TrackHash,
    /// Where in the source track this segment starts (samples at source
    /// rate); transitions cue into the track, so not always zero.
    pub src_start: u64,
    /// Session sample where this segment starts.
    pub session_start: SessionTime,
    /// Segment length in session samples (stretched time).
    pub len_samples: u64,
    /// Tempo-matching decision.
    pub stretch: StretchDecision,
    /// The transition out of this segment (into the next), if any.
    pub transition: Option<TransitionPlan>,
}

/// A fully planned continuous mix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPlan {
    /// The session-wide target tempo.
    pub session_bpm: f32,
    /// Engine sample rate all session times are expressed at.
    pub sample_rate: u32,
    /// Ordered segments; adjacent segments overlap during transitions.
    pub segments: Vec<Segment>,
}

impl SessionPlan {
    /// Total session length in samples (end of the final segment).
    #[must_use]
    pub fn total_len_samples(&self) -> u64 {
        self.segments
            .last()
            .map_or(0, |s| s.session_start.0 + s.len_samples)
    }

    /// The index of the segment audible at `time`, if any.
    ///
    /// During a transition two segments overlap; this returns the
    /// incoming (later) segment, since it is the one that becomes
    /// current as the window closes.
    #[must_use]
    pub fn segment_at(&self, time: SessionTime) -> Option<usize> {
        let candidates: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                time.0 >= s.session_start.0 && time.0 < s.session_start.0 + s.len_samples
            })
            .map(|(i, _)| i)
            .collect();
        candidates.last().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_time_converts_to_and_from_seconds() {
        // Given 1.5 seconds at 44.1 kHz.
        let t = SessionTime::from_seconds(1.5, 44_100);

        // Then it is 66_150 samples and round-trips.
        assert_eq!(t.0, 66_150);
        assert!((t.as_seconds(44_100) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn session_time_clamps_negative_seconds() {
        // Given a negative duration (defensive).
        let t = SessionTime::from_seconds(-2.0, 44_100);

        // Then it clamps to zero.
        assert_eq!(t, SessionTime::ZERO);
    }

    #[test]
    fn stretch_mode_selects_resample_within_band() {
        // Given ratios inside the ±8% band.
        // Then Resample is chosen.
        assert_eq!(StretchMode::for_ratio(1.0), StretchMode::Resample);
        assert_eq!(StretchMode::for_ratio(1.08), StretchMode::Resample);
        assert_eq!(StretchMode::for_ratio(0.92), StretchMode::Resample);
    }

    #[test]
    fn stretch_mode_selects_wsola_outside_band() {
        // Given ratios beyond ±8%.
        // Then WSOLA is chosen.
        assert_eq!(StretchMode::for_ratio(1.09), StretchMode::Wsola);
        assert_eq!(StretchMode::for_ratio(0.90), StretchMode::Wsola);
        assert_eq!(StretchMode::for_ratio(2.0), StretchMode::Wsola);
    }

    #[test]
    fn transition_window_length_is_end_minus_start() {
        // Given a window from sample 100 to 400.
        let w = TransitionWindow {
            start: SessionTime(100),
            end: SessionTime(400),
        };

        // Then the length is 300 samples.
        assert_eq!(w.len_samples(), 300);
        assert!(!w.is_empty());
    }

    #[test]
    fn cue_points_select_earliest_source_position_without_reordering_slots() {
        // Given cues whose earliest in-cue is stored in slot 4.
        let cues = CuePoints {
            ins: [Some(900), None, None, Some(300)],
            outs: [Some(700), None, Some(100), None],
        };

        // When selecting valid cues within a 1000-frame track.
        let selected_in = cues.earliest_valid(CueKind::In, 1_000);
        let selected_out = cues.earliest_valid(CueKind::Out, 1_000);

        // Then source position, not slot number, determines precedence.
        assert_eq!(selected_in, Some(300));
        assert_eq!(selected_out, Some(100));
    }

    #[test]
    fn cue_points_ignore_positions_after_track_end() {
        // Given an in-cue beyond the source duration.
        let cues = CuePoints {
            ins: [Some(1_001), None, None, None],
            ..CuePoints::default()
        };

        // When selecting a cue within a 1000-frame track.
        let selected = cues.earliest_valid(CueKind::In, 1_000);

        // Then the out-of-range cue is not valid.
        assert_eq!(selected, None);
    }

    #[test]
    fn cue_points_keep_slot_identity_when_editing() {
        // Given an empty cue set.
        let mut cues = CuePoints::default();

        // When setting and deleting a numbered slot.
        cues.set(CueKind::In, 3, 240);
        cues.delete(CueKind::In, 3);

        // Then only that slot is affected.
        assert_eq!(cues.get(CueKind::In, 3), None);
        assert!(cues.is_empty());
    }

    #[test]
    fn cue_points_ignore_out_of_range_slot_edits() {
        // Given an empty cue set.
        let mut cues = CuePoints::default();

        // When editing a slot outside the four-slot range.
        cues.set(CueKind::Out, CUE_SLOTS, 240);
        cues.delete(CueKind::Out, CUE_SLOTS);

        // Then the cue set remains empty.
        assert!(cues.is_empty());
    }

    #[test]
    fn cue_points_report_kind_presence_independently() {
        // Given one out-cue and no in-cues.
        let cues = CuePoints {
            outs: [Some(120), None, None, None],
            ..CuePoints::default()
        };

        // When checking each cue kind.
        // Then only the out-cue kind is present.
        assert!(!cues.has_any(CueKind::In));
        assert!(cues.has_any(CueKind::Out));
    }

    fn test_segment(hash: &str, start: u64, len: u64) -> Segment {
        Segment {
            track_hash: TrackHash(hash.to_string()),
            src_start: 0,
            session_start: SessionTime(start),
            len_samples: len,
            stretch: StretchDecision {
                mode: StretchMode::Resample,
                ratio: 1.0,
                out_of_comfort_band: false,
                strategy: TempoStrategy::SessionBpm,
            },
            transition: None,
        }
    }

    #[test]
    fn segment_at_returns_incoming_segment_during_overlap() {
        // Given overlapping segments A [0, 200) and B [150, 350).
        let plan = SessionPlan {
            session_bpm: 120.0,
            sample_rate: 44_100,
            segments: vec![test_segment("a", 0, 200), test_segment("b", 150, 200)],
        };

        // When asking for the segment at sample 175 (inside the overlap).
        let idx = plan.segment_at(SessionTime(175));

        // Then the incoming segment B is returned.
        assert_eq!(idx, Some(1));
    }

    #[test]
    fn segment_at_returns_outgoing_segment_before_overlap() {
        // Given overlapping segments A [0, 200) and B [150, 350).
        let plan = SessionPlan {
            session_bpm: 120.0,
            sample_rate: 44_100,
            segments: vec![test_segment("a", 0, 200), test_segment("b", 150, 200)],
        };

        // When asking for the segment at sample 100 (A only).
        let idx = plan.segment_at(SessionTime(100));

        // Then segment A is returned.
        assert_eq!(idx, Some(0));
    }

    #[test]
    fn total_len_is_end_of_final_segment() {
        // Given a plan whose last segment ends at 350.
        let plan = SessionPlan {
            session_bpm: 120.0,
            sample_rate: 44_100,
            segments: vec![test_segment("a", 0, 200), test_segment("b", 150, 200)],
        };

        // Then the total length is 350 samples.
        assert_eq!(plan.total_len_samples(), 350);
    }

    #[test]
    fn empty_plan_has_zero_length() {
        // Given an empty plan.
        let plan = SessionPlan {
            session_bpm: 120.0,
            sample_rate: 44_100,
            segments: vec![],
        };

        // Then total length is zero and no segment is audible.
        assert_eq!(plan.total_len_samples(), 0);
        assert_eq!(plan.segment_at(SessionTime(0)), None);
    }
}
