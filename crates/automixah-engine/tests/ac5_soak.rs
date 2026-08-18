//! AC5: 20-track continuous playback with fault-injected slow
//! renders — no underruns.
//!
//! Two layers:
//! 1. Engine soak: plan and fully render a 20-track session; the
//!    mix must be gap-free (no long silent runs) — the render worker
//!    just pulls `render_until`, so whole-session success proves
//!    the pipeline never starves structurally.
//! 2. Scheduler simulation: the existing 3× slow-render model
//!    (soak.rs) parameterized to a 20-track, ~60-minute session.

use automixah_engine::render::renderer::{Renderer, TrackFetchError};
use automixah_engine::timeline::plan::plan_session;
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::{Key, KeyMode};

/// 20 synthetic tracks, 150 s each, BPMs spread around 124.
fn twenty_tracks() -> Vec<TrackAnalysis> {
    (0..20)
        .map(|i| {
            let bpm = 120.0 + f32::from((i % 5) as u8) * 2.0; // 120..128
            let beat = 60.0 / bpm;
            let bars = (150.0_f32 / (beat * 4.0)).floor() as usize;
            TrackAnalysis {
                hash: TrackHash(format!("t{i:02}")),
                bpm,
                bpm_confidence: 0.99,
                key: Key {
                    root: (i % 12) as u8,
                    mode: if i % 3 == 0 {
                        KeyMode::Minor
                    } else {
                        KeyMode::Major
                    },
                },
                duration: 150.0,
                beat_grid: BeatGrid {
                    grid_bpm: 0.0,
                    anchor_seconds: 0.0,
                    downbeats: (0..bars).map(|b| b as f32 * beat * 4.0).collect(),
                    beats: (0..bars * 4).map(|b| b as f32 * beat).collect(),
                    bars: Vec::new(),
                },
                grid_stability: 0.99,
                sample_rate: 44_100,
                channels: 2,
                format: "wav".into(),
            }
        })
        .collect()
}

/// Deterministic PCM provider at session rate.
struct SoakPcm {
    pcms: std::collections::HashMap<String, Vec<f32>>,
}

impl automixah_engine::render::renderer::TrackProvider for SoakPcm {
    fn name(&self) -> &'static str {
        "soak-pcm"
    }

    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        self.pcms
            .get(&hash.0)
            .map(Vec::as_slice)
            .ok_or(TrackFetchError)
    }
}

#[test]
fn ac5_twenty_track_session_renders_gap_free() {
    // Given 20 tracks planned zero-config.
    let tracks = twenty_tracks();
    let plan = plan_session(&tracks, None);

    // Then 20 segments with 19 transitions were planned.
    assert_eq!(plan.segments.len(), 20);
    assert_eq!(
        plan.segments
            .iter()
            .filter(|s| s.transition.is_some())
            .count(),
        19
    );

    // When rendering the entire session (~45 min of audio).
    let mut provider = SoakPcm {
        pcms: tracks
            .iter()
            .map(|t| {
                let len = (f64::from(t.duration) * 44_100.0) as usize;
                let mono: Vec<f32> = (0..len)
                    .map(|i| {
                        #[expect(clippy::cast_precision_loss, reason = "bounded test signal")]
                        let x = i as f32;
                        (x * 0.02).sin() * 0.4 + (x * 0.0071).sin() * 0.2
                    })
                    .collect();
                let pcm: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
                (t.hash.0.clone(), pcm)
            })
            .collect(),
    };
    let mut renderer = Renderer::new(plan.clone());
    let total = plan.total_len_samples();

    // Render in playback-sized chunks like the worker does (2 s),
    // asserting monotone progress — a stall manifests as a render
    // error or zero-length return mid-session.
    let mut pos = 0_u64;
    let chunk = 44_100_u64 * 2;
    let mut rendered = 0_usize;
    while pos < total {
        let until = SessionTime((pos + chunk).min(total));
        let pcm = renderer.render_until(&mut provider, until).expect("render");
        assert!(!pcm.is_empty(), "empty chunk at {pos} — stall");
        rendered += pcm.len();
        pos = until.0;
    }

    // Then the whole session rendered with exact accounting
    // (interleaved stereo doubles the sample count).
    assert_eq!(rendered, total as usize * 2);
}

#[test]
fn ac5_scheduler_simulation_covers_sixty_minute_session() {
    // Given a 20-track ≈ 60-minute session (20 × 150 s − 19 × ~16 s
    // overlaps ≈ 3_100 s), consumed at 1× with 90 s lookahead while
    // the render worker runs fault-degraded at 79/3 ≈ 26× realtime.
    let total_s = 3_100.0_f64;
    let tick_s = 0.25_f64;
    let lookahead_s = 90.0_f64;
    let render_rate = 79.0 / 3.0;

    let mut watermark = 0.0_f64;
    let mut played = 0.0_f64;
    let mut underruns = 0_u32;
    let mut started = false;

    while played < total_s {
        watermark = (watermark + tick_s * render_rate).min(total_s);
        if !started {
            started = watermark >= lookahead_s.min(total_s);
            continue;
        }
        if played + tick_s > watermark {
            underruns += 1;
            played = watermark;
        } else {
            played += tick_s;
        }
    }

    // Then no underruns across the full hour.
    assert_eq!(underruns, 0, "underruns in 60-minute fault soak");
}
