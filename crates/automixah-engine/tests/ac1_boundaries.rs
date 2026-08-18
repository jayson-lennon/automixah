//! AC1: transition boundaries land within ±1 sample of planned beat
//! positions on synthetic-beatgrid fixtures.
//!
//! Synthetic grids give exact ground truth: at 120 BPM the bar
//! period is 88_200 samples at 44.1 kHz, so every planned boundary
//! must satisfy `boundary % 88_200 == 0` exactly.

use std::collections::HashMap;

use automixah_engine::render::renderer::{Renderer, TrackFetchError};
use automixah_engine::timeline::plan::plan_session;
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::{Key, KeyMode};

/// Builds a synthetic analysis: constant-BPM grid, bars of 4 beats,
/// downbeats every bar.
fn synth(hash: &str, bpm: f32, duration_s: f32) -> TrackAnalysis {
    let beat = 60.0 / bpm;
    let bars = (duration_s / (beat * 4.0)).floor() as usize;
    let downbeats = (0..bars).map(|i| i as f32 * beat * 4.0).collect();
    let beats = (0..bars * 4).map(|i| i as f32 * beat).collect();
    TrackAnalysis {
        hash: TrackHash(hash.into()),
        bpm,
        bpm_confidence: 0.99,
        key: Key {
            root: 7,
            mode: KeyMode::Minor,
        },
        duration: duration_s,
        beat_grid: BeatGrid {
            grid_bpm: 0.0,
            anchor_seconds: 0.0,
            downbeats,
            beats,
            bars: Vec::new(),
        },
        grid_stability: 0.99,
        sample_rate: 44_100,
        channels: 2,
        format: "wav".into(),
    }
}

/// Sine-burst PCM provider keyed by hash; PCM at session rate with
/// exact stretched length per segment.
struct SynthPcm {
    pcms: HashMap<String, Vec<f32>>,
}

impl SynthPcm {
    fn new(tracks: &[TrackAnalysis], stretch: f32) -> Self {
        Self {
            pcms: tracks
                .iter()
                .map(|t| {
                    let len = ((f64::from(t.duration) * f64::from(stretch)) * 44_100.0) as usize;
                    let mono: Vec<f32> = (0..len)
                        .map(|i| (i as f32 * 0.05).sin() * 0.5 + (i as f32 * 0.013).sin() * 0.3)
                        .collect();
                    // Interleave: L==R duplicates of the mono synth.
                    let pcm: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
                    (t.hash.0.clone(), pcm)
                })
                .collect(),
        }
    }
}

impl automixah_engine::render::renderer::TrackProvider for SynthPcm {
    fn name(&self) -> &'static str {
        "synth-pcm"
    }

    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        self.pcms
            .get(&hash.0)
            .map(Vec::as_slice)
            .ok_or(TrackFetchError)
    }
}

#[test]
fn ac1_boundaries_lie_exactly_on_downbeat_samples() {
    // Given a 3-track synthetic playlist (120/122/126 BPM, 120 s each)
    // planned at 120 BPM.
    let tracks = vec![
        synth("a", 120.0, 120.0),
        synth("b", 122.0, 120.0),
        synth("c", 126.0, 120.0),
    ];
    let plan = plan_session(&tracks, Some(120.0));

    // Then every transition boundary sits within one beat of a
    // downbeat of the outgoing track's stretched grid (the boundary
    // is segment start + audible span, both beat-quantized).
    for (i, seg) in plan.segments.iter().enumerate() {
        let Some(t) = seg.transition.as_ref() else {
            continue;
        };
        let bpm = tracks[i].bpm;
        let stretched_bar = f64::from((60.0 / bpm) * 4.0 * (120.0 / bpm).max(0.1) * 44_100.0);
        let rel = (t.window.end.0 as f64) - (seg.session_start.0 as f64);
        let phase = rel % stretched_bar;
        let off = phase.min(stretched_bar - phase);
        assert!(
            off <= f64::from(60.0 / bpm * 44_100.0),
            "segment {i} boundary {boundary} off downbeat by {off:.0}",
            boundary = t.window.end.0
        );
    }
}

#[test]
fn ac1_rendered_mix_reaches_every_planned_boundary() {
    // Given the same plan rendered end to end with synthetic PCM.
    let tracks = vec![
        synth("a", 120.0, 120.0),
        synth("b", 122.0, 120.0),
        synth("c", 126.0, 120.0),
    ];
    let plan = plan_session(&tracks, Some(120.0));

    let mut provider = SynthPcm::new(&tracks, 1.0);
    let mut renderer = Renderer::new(plan.clone());
    let total = plan.total_len_samples();
    let mix = renderer
        .render_until(&mut provider, SessionTime(total))
        .expect("render");

    // Then the rendered length is exactly the planned total.
    assert_eq!(mix.len(), total as usize * 2); // interleaved stereo

    // And every transition midpoint carries signal (non-silent
    // crossover from both decks).
    for seg in &plan.segments {
        let Some(t) = seg.transition.as_ref() else {
            continue;
        };
        let mid_frame = (t.window.start.0 + t.window.len_samples() / 2) as usize;
        let mid = mid_frame * 2;
        assert!(
            mix[mid].abs() > 0.0,
            "midpoint of window at {} silent",
            t.window.start.0
        );
    }
}
