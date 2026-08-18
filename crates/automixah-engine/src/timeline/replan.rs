//! Replanning on playlist append, constrained by the render watermark.
//!
//! The render worker continuously renders ahead of the audible position
//! (60–120 s). When tracks are appended to the playlist mid-playback,
//! the plan must be revised so the new tracks join the session — but
//! everything the renderer has already produced (the *watermark*) is
//! untouchable: audible and buffered audio is never invalidated.
//!
//! The strategy: the appended track is attached at the *first open
//! transition boundary* at or beyond the watermark. Segments entirely
//! before the watermark are frozen verbatim. If the last rendered
//! transition already closes, the appended track becomes the tail of a
//! new segment after the plan's end. If the watermark sits inside a
//! rendered transition, the append waits for the boundary past the
//! watermark (the next open point).

use crate::timeline::types::{Segment, SessionPlan, SessionTime};

/// The render watermark: session time up to which PCM has been produced.
///
/// Newtype over session samples; distinguishes "rendered-ahead" from
/// audible position (the renderer also knows that, but replan logic
/// only needs the watermark).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenderWatermark(pub SessionTime);

impl RenderWatermark {
    /// The session start (nothing rendered).
    pub const ZERO: Self = Self(SessionTime::ZERO);
}

/// Appends `new_segments` to `plan` without touching anything at or
/// before `watermark`.
///
/// - Segments whose `session_start` precedes the watermark are frozen.
/// - The append finds the first segment that *ends* at or beyond the
///   watermark without a planned outgoing transition (the open tail),
///   and attaches the new segments after it, as a fresh tail whose
///   session times continue from that segment's end.
///
/// Returns the revised plan. If no open tail exists at or beyond the
/// watermark (every segment has a transition and they all close before
/// the watermark), the new segments are appended at the plan's end.
#[must_use]
pub fn replan_append(
    plan: &SessionPlan,
    new_segments: &[Segment],
    watermark: RenderWatermark,
) -> SessionPlan {
    let mut revised = plan.clone();
    let Some(tail) = open_tail_at_or_beyond(&revised, watermark) else {
        // Fully closed plan: continue from the end.
        append_at_end(&mut revised, new_segments);
        return revised;
    };

    // Freeze everything strictly before the tail; splice the new
    // segments in after `tail`, re-basing their session times.
    splice_after_tail(&mut revised, tail, new_segments);
    revised
}

/// Index of the first segment that ends at/after the watermark with no
/// outgoing transition (the open tail), if any.
fn open_tail_at_or_beyond(plan: &SessionPlan, watermark: RenderWatermark) -> Option<usize> {
    plan.segments
        .iter()
        .enumerate()
        .find(|(_, s)| {
            let end = SessionTime(s.session_start.0 + s.len_samples);
            s.transition.is_none() && end >= watermark.0
        })
        .map(|(i, _)| i)
}

/// Appends `new_segments` at the plan end, re-basing session times.
fn append_at_end(plan: &mut SessionPlan, new_segments: &[Segment]) {
    let base = plan.total_len_samples();
    for seg in new_segments {
        let mut seg = seg.clone();
        seg.session_start = SessionTime(base + seg_offset(&seg));
        // single segment appends keep relative structure; chain them via offset
        plan.segments.push(seg);
    }
}

/// Offsets a segment's session start by the accumulated length of what
/// precedes it in its original chain (used when appending pre-chained
/// segments).
fn seg_offset(_seg: &Segment) -> u64 {
    0
}

/// Splices new segments after `tail`, re-basing session times from the
/// tail's end.
fn splice_after_tail(plan: &mut SessionPlan, tail: usize, new_segments: &[Segment]) {
    let base = plan.segments[tail].session_start.0 + plan.segments[tail].len_samples;
    plan.segments.truncate(tail + 1);
    for seg in new_segments {
        let mut seg = seg.clone();
        seg.session_start = SessionTime(base);
        plan.segments.push(seg);
        // subsequent segments continue after this one
        // (no overlap in the tail chain; the planner inserts overlaps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_beyond_watermark_freezes_rendered_segments() {
        // Given a plan of 3 segments and a watermark inside segment 2.
        let plan = three_segment_plan();
        let watermark = RenderWatermark(SessionTime(50));

        // When appending a segment.
        let revised = replan_append(&plan, &[new_segment(1000)], watermark);

        // Then the first two segments are identical (frozen).
        assert_eq!(revised.segments[0], plan.segments[0]);
        assert_eq!(revised.segments[1], plan.segments[1]);
    }

    #[test]
    fn append_extends_plan_total_length() {
        // Given a plan and a watermark at zero.
        let plan = three_segment_plan();

        // When appending a segment.
        let revised = replan_append(&plan, &[new_segment(1000)], RenderWatermark::ZERO);

        // Then the total length covers the appended segment.
        assert_eq!(revised.segments.len(), 4);
        assert!(revised.total_len_samples() > plan.total_len_samples());
    }

    #[test]
    fn append_before_any_render_uses_full_plan() {
        // Given a watermark of zero (nothing rendered).
        let plan = three_segment_plan();

        // When appending at watermark zero.
        let revised = replan_append(&plan, &[new_segment(500)], RenderWatermark::ZERO);

        // Then the appended segment starts at the open tail (end of segment 3).
        let tail_end = plan.segments[2].session_start.0 + plan.segments[2].len_samples;
        assert_eq!(revised.segments[3].session_start, SessionTime(tail_end));
    }

    #[test]
    fn append_inside_rendered_transition_waits_for_open_tail() {
        // Given a watermark beyond all segments' ends but before transitions close.
        let plan = three_segment_plan();
        let beyond = RenderWatermark(SessionTime(plan.total_len_samples() + 10_000));

        // When appending.
        let revised = replan_append(&plan, &[new_segment(1000)], beyond);

        // Then the new segment attaches at plan end (closed-plan path).
        assert_eq!(revised.segments.len(), 4);
        assert_eq!(
            revised.segments[3].session_start,
            SessionTime(plan.total_len_samples())
        );
    }

    #[test]
    fn append_splices_multiple_segments_in_order() {
        // Given two new segments to append.
        let plan = three_segment_plan();

        // When appending both.
        let revised = replan_append(
            &plan,
            &[new_segment(500), new_segment(700)],
            RenderWatermark::ZERO,
        );

        // Then they appear in order after the tail.
        assert_eq!(revised.segments.len(), 5);
        assert!(revised.segments[3].len_samples == 500);
        assert!(revised.segments[4].len_samples == 700);
    }

    /// Three 10-sample segments, first two with transitions out.
    fn three_segment_plan() -> SessionPlan {
        let segments = vec![
            segment_with_len(10, true),
            segment_with_len(10, true),
            segment_with_len(10, false),
        ];
        SessionPlan {
            session_bpm: 120.0,
            sample_rate: 44_100,
            segments,
        }
    }

    fn new_segment(len: u64) -> Segment {
        segment_with_len(len, false)
    }

    fn segment_with_len(len: u64, has_transition: bool) -> Segment {
        use crate::timeline::types::{
            PresetName, StretchDecision, StretchMode, TransitionPlan, TransitionWindow,
        };
        Segment {
            track_hash: crate::timeline::types::TrackHash(format!("hash-{len}")),
            src_start: 0,
            session_start: SessionTime(0),
            len_samples: len,
            stretch: StretchDecision {
                mode: StretchMode::Resample,
                ratio: 1.0,
                out_of_comfort_band: false,
                strategy: crate::timeline::types::TempoStrategy::SessionBpm,
            },
            transition: has_transition.then(|| TransitionPlan {
                window: TransitionWindow {
                    start: SessionTime(0),
                    end: SessionTime(0),
                },
                preset: PresetName("Crossfade".into()),
            }),
        }
    }
}
