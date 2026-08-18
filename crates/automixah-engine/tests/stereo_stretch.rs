//! Stereo independence through the frame-aware stretchers.

use automixah_engine::render::resample::Resampler;
use automixah_engine::render::wsola::Wsola;
use automixah_engine::timeline::types::{StretchDecision, StretchMode, TempoStrategy};

fn decision(ratio: f32, mode: StretchMode) -> StretchDecision {
    StretchDecision {
        ratio,
        mode,
        out_of_comfort_band: false,
        strategy: TempoStrategy::SessionBpm,
    }
}

/// Left-only interleaved stereo sine, 4 s.
fn left_only(len: usize) -> Vec<f32> {
    (0..len)
        .flat_map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "test index")]
            let l = (i as f32 * 0.03).sin() * 0.6;
            [l, 0.0]
        })
        .collect()
}

#[test]
fn resample_frames_keeps_left_only_left() {
    // Given a left-only stereo signal.
    let input = left_only(200_000);

    // When resampling at 1.02×.
    let out = Resampler::new(decision(1.02, StretchMode::Resample)).resample_all_frames(&input, 2);

    // Then the right plane stays silent and lengths are exact.
    assert_eq!(out.len() % 2, 0);
    assert_eq!(out.len(), (200_000_f64 * 1.02).round() as usize * 2);
    assert!(out.chunks(2).all(|f| f[1].abs() < 1e-6));
    assert!(out.chunks(2).any(|f| f[0].abs() > 0.3));
}

#[test]
fn wsola_frames_keeps_left_only_left() {
    // Given a left-only stereo signal.
    let input = left_only(200_000);

    // When time-stretching at 0.85×.
    let out = Wsola::new(decision(0.85, StretchMode::Wsola)).stretch_all_frames(&input, 2);

    // Then the right plane stays silent.
    assert_eq!(out.len(), (200_000_f64 * 0.85).round() as usize * 2);
    assert!(out.chunks(2).all(|f| f[1].abs() < 1e-6));
    assert!(out.chunks(2).any(|f| f[0].abs() > 0.3));
}
