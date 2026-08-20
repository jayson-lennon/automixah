//! Audibility integration: real overlapping mixes where both decks
//! are audible during the window and the crossfade trends correctly.

use automixah_engine::automation::transition_spec::{TransitionSpec, long_crossfade};
use automixah_engine::render::renderer::{Renderer, TrackFetchError, TrackProvider};
use automixah_engine::timeline::plan::{PlanOptions, plan_with};
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::key::{Key, KeyMode};

/// Two-tone synthetic track: analysis + PCM at a signature frequency.
struct SynthTrack {
    analysis: TrackAnalysis,
    pcm: Vec<f32>,
}

/// Builds a stereo synth track: confident grid at `bpm`, signature
/// sine at `hz`, duration `s` seconds.
fn synth(hash: &str, bpm: f32, hz: f32, s: f32) -> SynthTrack {
    let beat = 60.0 / bpm;
    let bar = beat * 4.0;
    let bars = (s / bar) as usize;
    let downbeats: Vec<f32> = (0..bars).map(|i| i as f32 * bar).collect();
    let beats: Vec<f32> = (0..bars * 4).map(|i| i as f32 * beat).collect();
    let frames = (s * 44_100.0) as usize;
    let pcm: Vec<f32> = (0..frames)
        .flat_map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "test index")]
            let t = i as f32 / 44_100.0;
            let v = (2.0 * std::f32::consts::PI * hz * t).sin() * 0.5;
            [v, v]
        })
        .collect();
    SynthTrack {
        analysis: TrackAnalysis {
            hash: TrackHash(hash.into()),
            bpm,
            bpm_confidence: 0.9,
            key: Key {
                root: 0,
                mode: KeyMode::Minor,
            },
            duration: s,
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
        },
        pcm,
    }
}

/// Provider that pre-stretches nothing (session BPM == track BPM so
/// ratio ≈ 1; resampler is still exact-length).
struct PcmProvider {
    pcms: std::collections::HashMap<String, Vec<f32>>,
}

impl TrackProvider for PcmProvider {
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

/// Goertzel magnitude of `hz` over interleaved channel `ch`.
fn goertzel(pcm: &[f32], ch: usize, hz: f32) -> f64 {
    let n = pcm.len() / 2;
    let k = 2.0 * std::f64::consts::PI * f64::from(hz) / 44_100.0;
    let (coeff, mut s1, mut s2) = (2.0 * k.cos(), 0.0_f64, 0.0_f64);
    for i in 0..n {
        let x = f64::from(pcm[i * 2 + ch]);
        let s = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power / n as f64
}

/// Mean of a Goertzel window (channel 0) over a frame span.
fn band_energy(pcm: &[f32], start_f: usize, len_f: usize, hz: f32) -> f64 {
    let seg = &pcm[(start_f * 2)..((start_f + len_f) * 2)];
    goertzel(seg, 0, hz)
}

#[test]
fn crossfade_is_audible_on_both_decks_with_correct_trend() {
    // Given two distinct-frequency 12 s tracks at nearby BPMs.
    let a = synth("a", 120.0, 220.0, 12.0);
    let b = synth("b", 121.0, 330.0, 12.0);
    let tracks = vec![a.analysis.clone(), b.analysis.clone()];

    // When planning + rendering the full session with the default pair.
    let plan = plan_with(
        &tracks,
        PlanOptions {
            transition_beats: 4,
            ..PlanOptions::default()
        },
    );
    let total = SessionTime(plan.total_len_samples());
    let mut provider = PcmProvider {
        pcms: [
            ("a".to_string(), a.pcm.clone()),
            ("b".to_string(), b.pcm.clone()),
        ]
        .into_iter()
        .collect(),
    };
    // Match the CLI provider: slice B from its stretched cue.
    {
        let cue_f = (plan.segments[1].src_start as f64 * f64::from(plan.segments[1].stretch.ratio))
            .round() as usize;
        let pcm = provider.pcms.get_mut("b").expect("b");
        *pcm = pcm[(cue_f * 2)..].to_vec();
    }
    let mut renderer = Renderer::with_transition(plan.clone(), long_crossfade());
    let mix = renderer.render_until(&mut provider, total).expect("render");

    // Then at the window midpoint both signatures are present.
    let w = plan.segments[0].transition.as_ref().expect("win");
    let mid_f = ((w.window.start.0 + w.window.len_samples() / 2) as usize).min(mix.len() / 2);
    let probe = 44_100 / 4; // quarter-second probe window
    let a_mid = band_energy(&mix, mid_f.saturating_sub(probe), probe, 220.0);
    let b_mid = band_energy(&mix, mid_f.saturating_sub(probe), probe, 330.0);
    assert!(a_mid > 1e-6, "outgoing deck silent at midpoint: {a_mid}");
    assert!(b_mid > 1e-6, "incoming deck silent at midpoint: {b_mid}");

    // And the trend across window thirds: A↓ B↑ monotonically.
    let third = w.window.len_samples() as usize / 3;
    let start_f = w.window.start.0 as usize;
    let mut a_energies = Vec::new();
    let mut b_energies = Vec::new();
    for k in 0..3 {
        let f0 = start_f + k * third;
        a_energies.push(band_energy(&mix, f0, third, 220.0));
        b_energies.push(band_energy(&mix, f0, third, 330.0));
    }
    assert!(
        a_energies[0] > a_energies[2],
        "outgoing energy must decay: {a_energies:?}"
    );
    assert!(
        b_energies[2] > b_energies[0],
        "incoming energy must rise: {b_energies:?}"
    );

    // And no clipping.
    assert!(mix.iter().all(|s| s.abs() <= 1.0));
}

#[test]
fn custom_pair_changes_the_output_envelope() {
    let a = synth("a", 120.0, 220.0, 12.0);
    let b = synth("b", 121.0, 330.0, 12.0);
    let tracks = vec![a.analysis.clone(), b.analysis.clone()];
    let plan = plan_with(
        &tracks,
        PlanOptions {
            transition_beats: 4,
            ..PlanOptions::default()
        },
    );
    let total = SessionTime(plan.total_len_samples());

    let render_with = |spec: TransitionSpec| -> Vec<f32> {
        let mut provider = PcmProvider {
            pcms: [
                ("a".to_string(), a.pcm.clone()),
                ("b".to_string(), b.pcm.clone()),
            ]
            .into_iter()
            .collect(),
        };
        let cue_f = (plan.segments[1].src_start as f64 * f64::from(plan.segments[1].stretch.ratio))
            .round() as usize;
        let pcm = provider.pcms.get_mut("b").expect("b");
        *pcm = pcm[(cue_f * 2)..].to_vec();
        let mut renderer = Renderer::with_transition(plan.clone(), spec);
        renderer.render_until(&mut provider, total).expect("render")
    };

    // When rendering with the default equal-power pair vs a snappy
    // linear pair (both 4 beats — the contrast is curve shape).
    let default_mix = render_with(long_crossfade());
    let mut snappy = long_crossfade();
    snappy.beats = 4;
    for c in &mut snappy.curves {
        c.shape = automixah_engine::automation::presets::Shape::Linear;
    }
    let snappy_mix = render_with(snappy);

    // Then the envelopes differ measurably at the default window's
    // midpoint (equal-power vs linear, long vs short windows).
    let w = plan.segments[0].transition.as_ref().expect("win");
    let mid_f = (w.window.start.0 + w.window.len_samples() / 2) as usize;
    let probe = 44_100 / 4;
    let rms = |mix: &[f32]| {
        let seg = &mix[(mid_f * 2)..((mid_f + probe) * 2)];
        (seg.iter().map(|s| f64::from(s * s)).sum::<f64>() / seg.len() as f64).sqrt()
    };
    let d = rms(&default_mix);
    let s = rms(&snappy_mix);
    assert!(
        (d - s).abs() > 1e-4,
        "custom pair must change the envelope: {d} vs {s}"
    );
}
