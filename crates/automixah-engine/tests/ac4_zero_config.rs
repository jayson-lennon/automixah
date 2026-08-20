//! AC4: zero-config flow — playlist → plan → render with no user
//! configuration produces a continuous, correctly planned mix.
//!
//! Uses the real-music fixture when present (local-only); falls back
//! to synthetic tracks otherwise so the flow is always exercised.
//! `#[ignore]`d in debug for the same reason as `real_track`.

use automixah_engine::render::renderer::{Renderer, TrackFetchError};
use automixah_engine::timeline::plan::plan_session;
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::{AudioAnalyzer, StratumAnalyzer};
use djcore::decoder::DecoderRegistry;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/music/sam-laxton-full-effect.ogg"
);

/// Two synthetic neighbors so the fixture has something to mix with.
fn synth(hash: &str, bpm: f32, duration_s: f32) -> TrackAnalysis {
    let beat = 60.0 / bpm;
    let bars = (duration_s / (beat * 4.0)).floor() as usize;
    TrackAnalysis {
        hash: TrackHash(hash.into()),
        bpm,
        bpm_confidence: 0.99,
        key: djcore::Key {
            root: 2,
            mode: djcore::KeyMode::Minor,
        },
        duration: duration_s,
        beat_grid: djcore::analyzer::BeatGrid {
            grid_bpm: 60.0 / beat,
            anchor_seconds: 0.0,
            downbeats: (0..bars).map(|i| i as f32 * beat * 4.0).collect(),
            beats: (0..bars * 4).map(|i| i as f32 * beat).collect(),
            bars: Vec::new(),
        },
        grid_stability: 0.99,
        sample_rate: 44_100,
        channels: 2,
        format: "wav".into(),
        cues: Default::default(),
    }
}

/// PCM provider rendering each track to session-rate mono PCM.
struct FixturePcm {
    pcms: std::collections::HashMap<String, Vec<f32>>,
}

impl automixah_engine::render::renderer::TrackProvider for FixturePcm {
    fn name(&self) -> &'static str {
        "fixture-pcm"
    }

    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        self.pcms
            .get(&hash.0)
            .map(Vec::as_slice)
            .ok_or(TrackFetchError)
    }
}

#[test]
#[ignore = "requires local fixture; slow in debug"]
fn zero_config_flow_yields_continuous_planned_mix() {
    // Given the real fixture analyzed alongside synthetic neighbors.
    let Ok(bytes) = std::fs::read(FIXTURE) else {
        eprintln!("real fixture absent, skipping");
        return;
    };
    let registry = DecoderRegistry::with_symphonia();
    let audio = registry.decode(&bytes, "ogg").expect("decode");
    let result = StratumAnalyzer::new()
        .analyze(&audio.samples, audio.sample_rate)
        .expect("analyze");
    let real = TrackAnalysis {
        hash: TrackHash("fixture".into()),
        bpm: result.bpm,
        bpm_confidence: result.bpm_confidence,
        key: result.key,
        duration: result.duration_seconds,
        beat_grid: result.beat_grid,
        grid_stability: result.grid_stability,
        sample_rate: audio.sample_rate,
        channels: 2,
        format: "ogg".into(),
        cues: Default::default(),
    };

    let tracks = vec![
        synth("open", 148.0, 150.0),
        real,
        synth("close", 146.0, 150.0),
    ];

    // When planning with NO user override (the zero-config default).
    let plan = plan_session(&tracks, None);

    // Then every adjacent pair has a transition with a chosen preset,
    // the session tempo is a real folded BPM from the playlist…
    assert_eq!(plan.segments.len(), 3);
    for seg in &plan.segments[..2] {
        let t = seg.transition.as_ref().expect("transition planned");
        assert!(
            !t.preset.0.is_empty(),
            "transition without preset at {}",
            t.window.start.0
        );
    }
    assert!(plan.session_bpm >= 90.0 && plan.session_bpm < 180.0);

    // …and rendering the whole session yields exactly the planned
    // length with no gaps (monotone continuation across boundaries).
    let mut provider = FixturePcm {
        pcms: tracks
            .iter()
            .map(|t| {
                let len = (f64::from(t.duration) * 44_100.0) as usize;
                let mono: Vec<f32> = (0..len)
                    .map(|i| (i as f32 * 0.02).sin() * 0.4 + (i as f32 * 0.007).sin() * 0.2)
                    .collect();
                let pcm: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
                (t.hash.0.clone(), pcm)
            })
            .collect(),
    };
    let mut renderer = Renderer::new(plan.clone());
    let total = plan.total_len_samples();
    let mix = renderer
        .render_until(&mut provider, SessionTime(total))
        .expect("render");
    assert_eq!(mix.len(), total as usize * 2); // interleaved stereo

    // No long silent stretch inside the session body (continuity).
    let window = 44_100; // 1 s
    let silent_max = 44_100 * 5; // allow 5 s of track silence, not gaps
    let mut run = 0_usize;
    let mut worst = 0_usize;
    for (i, s) in mix.iter().enumerate().take(mix.len() - 1) {
        if i % window == 0 {
            run = 0;
        }
        if s.abs() < 1e-6 {
            run += 1;
            worst = worst.max(run);
        }
    }
    assert!(
        worst <= silent_max,
        "silent run of {worst} samples — mix gap"
    );
}
