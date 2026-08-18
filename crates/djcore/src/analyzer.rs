//! Audio analysis using stratum-dsp.
//!
//! Provides an [`AudioAnalyzer`] trait for BPM, key, duration, and beat-grid
//! analysis, along with a [`StratumAnalyzer`] implementation wrapping the
//! stratum-dsp crate. Unlike the original harmonic-playlist analyzer (which
//! discarded the beat grid), this module surfaces the full grid — downbeats,
//! beats, and bars — plus `grid_stability` and `bpm_confidence`, which the
//! automixah session planner needs for phrase-aligned transitions.

use std::sync::atomic::{AtomicUsize, Ordering};

use error_stack::{Report, ResultExt};
use wherror::Error;

use crate::key::{Key, KeyMode};
use stratum_dsp::{AnalysisConfig, Key as StratumKey};

/// Errors that can occur during audio analysis.
#[derive(Debug, Error)]
#[error("audio analysis error")]
pub struct AnalyzerError;

/// Beat grid extracted from a track.
///
/// All times are in seconds from the start of the track. Beat times come
/// from stratum-dsp's onset/tempogram analysis; downbeats are the detected
/// beat-1 positions (phrase anchors for transitions); bars are the
/// four-beat bar boundaries.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BeatGrid {
    /// Canonical constant-tempo BPM (rounded by the fit ladder).
    /// Zero means "no grid" (unconfident fallback).
    pub grid_bpm: f32,
    /// Phase anchor: a downbeat time in `[0, bar)` seconds; the
    /// arrays below are projections of `anchor + k·60/grid_bpm`.
    pub anchor_seconds: f32,
    /// Downbeat times (beat 1 of each bar) in seconds.
    pub downbeats: Vec<f32>,
    /// All beat times in seconds.
    pub beats: Vec<f32>,
    /// Bar boundary times in seconds.
    pub bars: Vec<f32>,
}

impl From<stratum_dsp::BeatGrid> for BeatGrid {
    fn from(other: stratum_dsp::BeatGrid) -> Self {
        Self {
            grid_bpm: other.grid_bpm,
            anchor_seconds: other.anchor_seconds,
            downbeats: other.downbeats,
            beats: other.beats,
            bars: other.bars,
        }
    }
}

/// Full output from audio analysis.
///
/// Everything the session planner and UI need about a track, in one
/// serializable value (persisted to OPFS as JSON by the automixah
/// analysis worker).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnalyzerOutput {
    /// Detected tempo in beats per minute.
    pub bpm: f32,
    /// Detected musical key.
    pub key: Key,
    /// Duration of the audio in seconds.
    pub duration_seconds: f32,
    /// Beat grid (downbeats, beats, bars).
    pub beat_grid: BeatGrid,
    /// BPM detection confidence in `[0, 1]`.
    pub bpm_confidence: f32,
    /// Key detection confidence in `[0, 1]`.
    pub key_confidence: f32,
    /// Beat-grid stability in `[0, 1]`.
    pub grid_stability: f32,
}

/// Trait for audio analysis backends.
///
/// Implementors provide BPM, key, duration, and beat-grid analysis from
/// mono normalized samples.
pub trait AudioAnalyzer: Send + Sync {
    /// Returns the name of this analyzer for debugging.
    fn name(&self) -> &'static str;

    /// Analyze audio samples and return the full analysis output.
    ///
    /// # Errors
    ///
    /// Returns an error if analysis fails.
    fn analyze(
        &self,
        samples: &[f32],
        sample_rate: u32,
    ) -> Result<AnalyzerOutput, Report<AnalyzerError>>;
}

/// Audio analyzer backed by the stratum-dsp crate.
///
/// Analyzes mono samples for BPM, key, duration, and the beat grid using
/// stratum-dsp's tempogram, key-detection, and grid-construction
/// algorithms with the default configuration.
#[derive(Debug, Clone, Default)]
pub struct StratumAnalyzer;

impl StratumAnalyzer {
    /// Create a new stratum analyzer with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl AudioAnalyzer for StratumAnalyzer {
    fn name(&self) -> &'static str {
        "stratum-dsp"
    }

    fn analyze(
        &self,
        samples: &[f32],
        sample_rate: u32,
    ) -> Result<AnalyzerOutput, Report<AnalyzerError>> {
        let config = AnalysisConfig::default();
        let result = stratum_dsp::analyze_audio(samples, sample_rate, config)
            .change_context(AnalyzerError)
            .attach("stratum-dsp analysis failed")?;

        Ok(AnalyzerOutput {
            bpm: result.bpm,
            key: Key::from(result.key),
            duration_seconds: result.metadata.duration_seconds,
            beat_grid: BeatGrid::from(result.beat_grid),
            bpm_confidence: result.bpm_confidence,
            key_confidence: result.key_confidence,
            grid_stability: result.grid_stability,
        })
    }
}

/// Convert a stratum-dsp key to the djcore domain key.
impl From<StratumKey> for Key {
    fn from(other: StratumKey) -> Self {
        match other {
            StratumKey::Major(root) => Key {
                root: (root % 12) as u8,
                mode: KeyMode::Major,
            },
            StratumKey::Minor(root) => Key {
                root: (root % 12) as u8,
                mode: KeyMode::Minor,
            },
        }
    }
}

/// A fake audio analyzer for testing.
///
/// Returns the constructor-provided values verbatim and tracks the number
/// of analyze calls.
pub struct FakeAnalyzer {
    call_count: AtomicUsize,
    output: AnalyzerOutput,
}

impl FakeAnalyzer {
    /// Create a new fake analyzer returning the given output.
    #[must_use]
    pub fn with_output(output: AnalyzerOutput) -> Self {
        Self {
            call_count: AtomicUsize::new(0),
            output,
        }
    }

    /// Returns the number of times analyze has been called.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl AudioAnalyzer for FakeAnalyzer {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn analyze(
        &self,
        _samples: &[f32],
        _sample_rate: u32,
    ) -> Result<AnalyzerOutput, Report<AnalyzerError>> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(self.output.clone())
    }
}

impl Default for FakeAnalyzer {
    fn default() -> Self {
        Self::with_output(AnalyzerOutput {
            bpm: 120.0,
            key: Key {
                root: 9, // A
                mode: KeyMode::Minor,
            },
            duration_seconds: 180.0,
            beat_grid: BeatGrid::default(),
            bpm_confidence: 1.0,
            key_confidence: 1.0,
            grid_stability: 1.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_analyzer_returns_constructed_output() {
        // Given a fake analyzer with a specific output.
        let output = AnalyzerOutput {
            bpm: 128.0,
            key: Key::parse("C").expect("valid key"),
            duration_seconds: 42.0,
            beat_grid: BeatGrid {
                grid_bpm: 120.0,
                anchor_seconds: 0.0,
                downbeats: vec![0.0, 2.0],
                beats: vec![0.0, 0.5, 1.0, 1.5],
                bars: vec![0.0, 2.0],
            },
            bpm_confidence: 0.9,
            key_confidence: 0.8,
            grid_stability: 0.7,
        };
        let analyzer = FakeAnalyzer::with_output(output.clone());

        // When analyzing any samples.
        let result = analyzer.analyze(&[0.0; 16], 44_100).expect("analysis");

        // Then the constructed output comes back verbatim.
        assert!((result.bpm - 128.0).abs() < f32::EPSILON);
        assert_eq!(result.key, Key::parse("C").expect("valid key"));
        assert!((result.duration_seconds - 42.0).abs() < f32::EPSILON);
        assert_eq!(result.beat_grid.downbeats, vec![0.0, 2.0]);
        assert_eq!(result.beat_grid.beats, vec![0.0, 0.5, 1.0, 1.5]);
        assert!((result.bpm_confidence - 0.9).abs() < f32::EPSILON);
        assert!((result.grid_stability - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn fake_analyzer_counts_calls() {
        // Given a default fake analyzer.
        let analyzer = FakeAnalyzer::default();

        // When analyzing twice.
        let _ = analyzer.analyze(&[], 44_100);
        let _ = analyzer.analyze(&[], 44_100);

        // Then the call count is two.
        assert_eq!(analyzer.call_count(), 2);
    }

    #[test]
    fn output_roundtrips_through_serde() {
        // Given a fully populated analysis output.
        let output = AnalyzerOutput {
            bpm: 124.5,
            key: Key::parse("F#m").expect("valid key"),
            duration_seconds: 217.25,
            beat_grid: BeatGrid {
                grid_bpm: 124.0,
                anchor_seconds: 0.0,
                downbeats: vec![0.0, 1.935, 3.871],
                beats: vec![0.0, 0.484],
                bars: vec![0.0, 1.935],
            },
            bpm_confidence: 0.83,
            key_confidence: 0.71,
            grid_stability: 0.92,
        };

        // When serializing to JSON and back.
        let json = serde_json::to_string(&output).expect("serialize");
        let roundtripped: AnalyzerOutput = serde_json::from_str(&json).expect("deserialize");

        // Then the value survives intact.
        assert!((roundtripped.bpm - output.bpm).abs() < f32::EPSILON);
        assert_eq!(roundtripped.key, output.key);
        assert_eq!(roundtripped.beat_grid.downbeats, output.beat_grid.downbeats);
    }
}
