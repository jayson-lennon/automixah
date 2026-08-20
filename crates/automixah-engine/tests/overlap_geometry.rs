//! Overlap geometry: incoming segments start at the window start,
//! session total reflects overlaps, cues land on downbeats.

use automixah_engine::timeline::plan::PlanOptions;
use automixah_engine::timeline::plan::plan_session;
use automixah_engine::timeline::plan::plan_with;
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::key::{Key, KeyMode};

/// Synthetic analysis with a perfectly regular confident grid.
fn track(hash: &str, bpm: f32, duration: f32) -> TrackAnalysis {
    let beat = 60.0 / bpm;
    let bar = beat * 4.0;
    let bars = (duration / bar) as usize;
    let downbeats: Vec<f32> = (0..bars).map(|i| i as f32 * bar).collect();
    let beats: Vec<f32> = (0..bars * 4).map(|i| i as f32 * beat).collect();
    TrackAnalysis {
        hash: automixah_engine::timeline::types::TrackHash(hash.into()),
        bpm,
        bpm_confidence: 0.9,
        key: Key {
            root: 0,
            mode: KeyMode::Minor,
        },
        duration,
        beat_grid: BeatGrid {
            grid_bpm: 0.0,
            anchor_seconds: 0.0,
            downbeats,
            beats,
            bars: Vec::new(),
        },
        grid_stability: 0.9,
        sample_rate: 44_100,
        channels: 2,
        format: String::new(),
    }
}

#[test]
fn incoming_segment_starts_at_window_start() {
    // Given a confident-grid pair at 120 BPM, 240 s each.
    let tracks = vec![track("a", 120.0, 240.0), track("b", 122.0, 240.0)];

    // When planning.
    let plan = plan_with(&tracks, PlanOptions::default());

    // Then segment 1 starts exactly at segment 0's window start.
    let w = plan.segments[0].transition.as_ref().expect("transition");
    assert_eq!(plan.segments[1].session_start, w.window.start);
    assert!(w.window.start.0 < plan.segments[0].len_samples);
}

#[test]
fn session_total_is_sum_minus_overlaps() {
    // Given three tracks.
    let tracks = vec![
        track("a", 120.0, 240.0),
        track("b", 122.0, 240.0),
        track("c", 121.0, 240.0),
    ];

    // When planning.
    let plan = plan_with(&tracks, PlanOptions::default());

    // Then total = last start + last length, and each adjacent pair
    // overlaps: B starts strictly before A ends.
    let last = plan.segments.last().expect("last");
    assert_eq!(
        plan.total_len_samples(),
        last.session_start.0 + last.len_samples
    );

    for pair in plan.segments.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        let a_end = a.session_start.0 + a.len_samples;
        assert!(
            b.session_start.0 < a_end,
            "segments abut instead of overlapping: b starts at {}, a ends at {a_end}",
            b.session_start.0
        );
    }
}

#[test]
fn confident_grid_cues_on_first_downbeat() {
    // Given a track whose first downbeat sits at bar 2 (4 s in).
    let b = offset_grid(track("b", 120.0, 240.0), 4.0);
    let tracks = vec![track("a", 120.0, 240.0), b];

    // When planning.
    let plan = plan_with(&tracks, PlanOptions::default());

    // Then the incoming cue is the first downbeat exactly.
    let seg = &plan.segments[1];
    #[expect(clippy::cast_precision_loss, reason = "test cue seconds")]
    let cue_s = seg.src_start as f64 / 44_100.0;
    assert!(
        (cue_s - 4.0).abs() <= 1e-3,
        "cue {cue_s}s is not the first downbeat at 4.0s"
    );
}

/// Shifts every downbeat and beat later by `offset` seconds.
fn offset_grid(mut t: TrackAnalysis, offset: f32) -> TrackAnalysis {
    t.beat_grid.downbeats = t.beat_grid.downbeats.iter().map(|&d| d + offset).collect();
    t.beat_grid.beats = t.beat_grid.beats.iter().map(|&b| b + offset).collect();
    t.beat_grid.anchor_seconds = offset;
    t
}

#[test]
fn unconfident_grid_falls_back_to_zero_cue() {
    // Given an incoming track with no downbeats and low stability.
    let mut b = track("b", 120.0, 240.0);
    b.beat_grid = BeatGrid::default();
    b.grid_stability = 0.1;
    let tracks = vec![track("a", 120.0, 240.0), b];

    // When planning.
    let plan = plan_with(&tracks, PlanOptions::default());

    // Then the cue is zero (time-based fallback) and the session
    // still plans two overlapping segments.
    assert_eq!(plan.segments[1].src_start, 0);
    assert_eq!(plan.segments.len(), 2);
}

#[test]
fn unconfident_grid_window_stays_inside_stretched_audio() {
    // Given a 363 s track at 140 BPM planned into a 150 BPM session
    // with an *unconfident* grid (stability 0.0) — the exact bug that
    // produced a silent gap between real tracks.
    let track = TrackAnalysis {
        hash: TrackHash("x".into()),
        bpm: 140.0,
        bpm_confidence: 0.15,
        key: Key {
            root: 0,
            mode: KeyMode::Minor,
        },
        duration: 363.0,
        beat_grid: BeatGrid {
            grid_bpm: 0.0,
            anchor_seconds: 0.0,
            downbeats: Vec::new(),
            beats: Vec::new(),
            bars: Vec::new(),
        },
        grid_stability: 0.0,
        sample_rate: 44_100,
        channels: 2,
        format: String::new(),
    };
    let next = TrackAnalysis {
        hash: TrackHash("y".into()),
        bpm: 140.0,
        bpm_confidence: 0.02,
        key: Key {
            root: 0,
            mode: KeyMode::Minor,
        },
        duration: 364.0,
        beat_grid: BeatGrid {
            grid_bpm: 0.0,
            anchor_seconds: 0.0,
            downbeats: Vec::new(),
            beats: Vec::new(),
            bars: Vec::new(),
        },
        grid_stability: 0.02,
        sample_rate: 44_100,
        channels: 2,
        format: String::new(),
    };

    // When planning at 150 BPM.
    let plan = plan_session(&[track, next], Some(150.0));

    // Then the window ends no later than the outgoing segment's
    // stretched end (≈339 s), never at the native 363 s.
    let seg = &plan.segments[0];
    let stretched_end = seg.session_start.0 + seg.len_samples;
    let w = seg.transition.as_ref().expect("window");
    assert!(
        w.window.end.0 <= stretched_end,
        "window end {} beyond stretched audio {}",
        w.window.end.0,
        stretched_end
    );
    assert!(w.window.start.0 > 0, "window must start inside the track");
}

#[test]
fn confident_grid_closes_window_at_last_downbeat_with_audio() {
    // Given a 120 BPM outgoing track (2 s bars) with its last
    // downbeat-with-audio at 236 s in a 239 s track (238 + margin
    // exceeds the audio), stretched 1:1 at session 120.
    let a = track("a", 120.0, 239.0);
    let tracks = vec![a, track("b", 120.0, 240.0)];

    // When planning at 120.
    let plan = plan_session(&tracks, Some(120.0));

    // Then the window ends at the stretched last downbeat (236 s),
    // not at the full track end (239 s).
    let seg = &plan.segments[0];
    let w = seg.transition.as_ref().expect("window");
    let expected = SessionTime::from_seconds(236.0, 44_100).0;
    assert!(
        (w.window.end.0 as i64 - expected as i64).abs() <= 44_100 / 100,
        "window end {} is not the last downbeat with audio {}",
        w.window.end.0,
        expected
    );
}
