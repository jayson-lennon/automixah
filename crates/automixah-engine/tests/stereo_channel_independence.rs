//! AC: stereo WAV output — left/right content stays distinguishable.

use automixah_engine::automation::transition_spec::long_crossfade;
use automixah_engine::render::renderer::{Renderer, TrackFetchError, TrackProvider};
use automixah_engine::timeline::plan::PlanOptions;
use automixah_engine::timeline::plan::plan_with;
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::key::{Key, KeyMode};

/// Left-only track A + right-only track B, then assert the mix's
/// channels carry their respective signatures after the crossfade.
struct SideProvider {
    a: Vec<f32>,
    b: Vec<f32>,
}

impl TrackProvider for SideProvider {
    fn name(&self) -> &'static str {
        "side-pcm"
    }
    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        if hash.0 == "a" {
            Ok(&self.a)
        } else {
            Ok(&self.b)
        }
    }
}

fn analysis(hash: &str, bpm: f32) -> TrackAnalysis {
    let beat = 60.0 / bpm;
    let bars = 60;
    let downbeats: Vec<f32> = (0..bars).map(|i| i as f32 * beat * 4.0).collect();
    let beats: Vec<f32> = (0..bars * 4).map(|i| i as f32 * beat).collect();
    TrackAnalysis {
        hash: TrackHash(hash.into()),
        bpm,
        bpm_confidence: 0.9,
        key: Key {
            root: 0,
            mode: KeyMode::Minor,
        },
        duration: bars as f32 * beat * 4.0,
        beat_grid: BeatGrid {
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

fn tone_pcm(frames: usize, hz: f32, side: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let t = i as f32 / 44_100.0;
            let v = (2.0 * std::f32::consts::PI * hz * t).sin() * 0.5;
            if side == 0 { [v, 0.0] } else { [0.0, v] }
        })
        .collect()
}

#[test]
fn left_only_and_right_only_sources_stay_distinguishable() {
    // Given A is left-only 220 Hz, B is right-only 330 Hz (120 BPM).
    let analyses = vec![analysis("a", 120.0), analysis("b", 120.0)];
    let plan = plan_with(&analyses, PlanOptions::default());
    let mut provider = SideProvider {
        a: tone_pcm(44_100 * 120, 220.0, 0),
        b: tone_pcm(44_100 * 120, 330.0, 1),
    };
    // Slice B from its stretched cue (ratio 1.0 here).
    let cue_f = plan.segments[1].src_start as usize;
    provider.b = provider.b[(cue_f * 2)..].to_vec();

    // When rendering.
    let mut renderer = Renderer::with_transition(plan.clone(), long_crossfade());
    let mix = renderer
        .render_until(&mut provider, SessionTime(plan.total_len_samples()))
        .expect("render");

    // Then post-transition (B alone) the right channel dominates and
    // the left is silent: the sides stayed distinguishable through
    // decode-side conventions (renderer preserves interleaving).
    let w = plan.segments[0].transition.as_ref().expect("win");
    let post_f = (w.window.end.0 + 44_100) as usize;
    let (mut l_energy, mut r_energy) = (0.0_f64, 0.0_f64);
    for frame in mix[(post_f * 2)..].chunks(2) {
        l_energy += f64::from(frame[0] * frame[0]);
        r_energy += f64::from(frame[1] * frame[1]);
    }
    assert!(
        r_energy > l_energy * 10.0,
        "right must dominate post-mix: {r_energy} vs {l_energy}"
    );
    assert!(
        r_energy > 1e-3,
        "right channel must carry signal: {r_energy}"
    );
}
