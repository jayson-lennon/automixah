//! Beat tracking modules
//!
//! Generate a precise constant-tempo beat grid from a BPM
//! estimate:
//! - Ellis-style DP beat marking over a novelty envelope
//! - Mixxx-style constant-grid fit (ironing, BPM rounding,
//!   phase anchor, downbeat vote)
//!
//! # Overview
//!
//! This module provides beat tracking functionality to convert BPM
//! estimates into precise beat grids. The main entry point is
//! `generate_beat_grid()`, which marks beats with the DP tracker
//! and then fits a single constant grid (one rounded BPM + one
//! phase anchor) to those marks.
//!
//! # Algorithm Pipeline
//!
//! 1. **DP Beat Marking**: optimal beat path over the novelty
//!    envelope, seeded by the tempogram BPM
//! 2. **Constant-Grid Fit**: region ironing, BPM regression with
//!    musical rounding, phase-anchor adjustment
//! 3. **Downbeat Vote**: bar phase chosen by novelty energy
//! 4. **Grid Stability**: phase consistency of marks vs grid
//!
//! # Example
//!
//! ```no_run
//! use stratum_dsp::features::beat_tracking::generate_beat_grid;
//!
//! let bpm_estimate = 120.0;
//! let novelty = vec![0.0, 0.1, 0.9, 0.0, 0.1, 0.8]; // per-hop envelope
//! let hop_size = 512;
//! let sample_rate = 44100;
//! let duration = 60.0;
//!
//! let (beat_grid, stability) = generate_beat_grid(
//!     bpm_estimate,
//!     0.85,
//!     &novelty,
//!     hop_size,
//!     sample_rate,
//!     duration,
//! )?;
//!
//! println!("Beat grid: {} beats, {} downbeats, stability={:.2}",
//!          beat_grid.beats.len(), beat_grid.downbeats.len(), stability);
//! # Ok::<(), stratum_dsp::AnalysisError>(())
//! ```

pub mod dp;
pub mod grid_fit;
pub mod time_signature;

use crate::analysis::result::BeatGrid;
use crate::error::AnalysisError;

/// Generates a constant-tempo beat grid from a novelty envelope.
///
/// This is the main public API for beat tracking. It marks beats
/// with the Ellis-style dynamic-programming tracker (seeded by the
/// tempogram BPM), then fits a single constant grid — one rounded
/// BPM and one phase anchor — to those marks (Mixxx-style region
/// ironing, BPM regression with a rounding ladder, and phase
/// adjustment). The `beats`/`downbeats`/`bars` arrays are gapless
/// projections of that grid.
///
/// # Arguments
///
/// * `bpm_estimate` - BPM estimate from the tempogram (seed for
///   the DP period prior)
/// * `_bpm_confidence` - Confidence in the BPM estimate (diagnostics)
/// * `novelty` - Onset novelty envelope, one value per STFT hop
/// * `hop_size` - Analysis hop size in samples
/// * `_sample_rate` - Sample rate in Hz (for logging)
/// * `duration_seconds` - Duration of the (trimmed) audio
///
/// # Returns
///
/// Tuple of `(BeatGrid, grid_stability)` where `grid_stability`
/// is the phase consistency of the marks against the fitted grid.
///
/// # Errors
///
/// Returns `AnalysisError` when the envelope cannot yield a grid
/// (too few marks, degenerate tempo fit).
pub fn generate_beat_grid(
    bpm_estimate: f32,
    _bpm_confidence: f32,
    novelty: &[f32],
    hop_size: u32,
    _sample_rate: u32,
    duration_seconds: f32,
) -> Result<(BeatGrid, f32), AnalysisError> {
    if !(0.0..=300.0).contains(&bpm_estimate) {
        return Err(AnalysisError::InvalidInput(format!(
            "Invalid BPM estimate: {:.2}",
            bpm_estimate
        )));
    }
    if hop_size == 0 {
        return Err(AnalysisError::InvalidInput(
            "Invalid hop size: 0".to_string(),
        ));
    }

    let hop_seconds = f64::from(hop_size) / f64::from(_sample_rate);
    let marks = dp::track_beats_dp(novelty, hop_seconds, bpm_estimate);
    log::debug!(
        "DP beat tracker: {} marks from {} envelope frames (seed {:.2} BPM)",
        marks.len(),
        novelty.len(),
        bpm_estimate
    );

    grid_fit::fit_constant_grid(&marks, bpm_estimate, novelty, hop_seconds, duration_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Impulse novelty envelope with a click every beat period.
    fn click_env(beats: usize, bpm: f32, hop: f64) -> Vec<f32> {
        let period = 60.0 / f64::from(bpm);
        let frames = ((period * beats as f64) / hop).ceil() as usize + 8;
        let mut env = vec![0.0_f32; frames];
        for b in 0..beats {
            let frame = ((period * b as f64) / hop).round() as usize;
            env[frame.min(frames - 1)] = 1.0;
        }
        env
    }

    #[test]
    fn generates_constant_grid_from_click_envelope() {
        // Given 64 clicks at 138 BPM.
        let hop = 1024.0 / 44_100.0;
        let env = click_env(64, 138.0, hop);

        // When generating the grid.
        let (grid, stability) =
            generate_beat_grid(138.0, 0.9, &env, 1024, 44_100, 64.0 * 60.0 / 138.0).expect("grid");

        // Then the grid is constant with rounded BPM and dense beats.
        assert!((grid.grid_bpm - 138.0).abs() < f32::EPSILON);
        assert!(stability > 0.9);
        assert!(!grid.beats.is_empty());
        assert!(!grid.downbeats.is_empty());
    }

    #[test]
    fn too_few_marks_is_an_error() {
        // Given an envelope that can only yield a handful of marks.
        let hop = 1024.0 / 44_100.0;
        let env = click_env(6, 138.0, hop);

        // When generating the grid.
        let result = generate_beat_grid(138.0, 0.9, &env, 1024, 44_100, 3.0);

        // Then it errors (caller falls back to an unconfident grid).
        assert!(result.is_err());
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        // Given a minimal envelope.
        let env = vec![1.0_f32; 4096];

        // When generating with invalid BPM or hop.
        // Then it errors.
        assert!(generate_beat_grid(0.0, 0.8, &env, 1024, 44_100, 10.0).is_err());
        assert!(generate_beat_grid(350.0, 0.8, &env, 1024, 44_100, 10.0).is_err());
        assert!(generate_beat_grid(138.0, 0.8, &env, 0, 44_100, 10.0).is_err());
    }
}
