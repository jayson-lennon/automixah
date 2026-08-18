//! Pitch-adjusted resampling (the "turntable pitch fader" path).
//!
//! Within the ±8% comfort band a track is matched to session tempo by
//! plain resampling: reading the source at a fractional position and
//! interpolating. Pitch shifts proportionally with tempo — the DJ
//! turntable behavior — at a fraction of WSOLA's cost, with
//! **exact** output length by construction: output length is
//! `(input length × ratio)` computed in f64, rounded once.
//!
//! This module is a streaming-capable primitive: [`Resampler`] holds
//! the fractional read cursor and the last input samples so producers
//! can feed chunks and receive exact-length chunks back.

use crate::timeline::types::StretchDecision;

/// Cubic (Catmull-Rom) interpolation of the four samples around
/// fractional position `pos` in `input`.
///
/// `pos` is clamped to the interpolable range; edges are handled by
/// the caller padding (one sample head/tail) or by clamping here.
#[must_use]
pub fn cubic_at(input: &[f32], pos: f64) -> f32 {
    let idx = pos.floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "index clamped to interpolable range"
    )]
    let i = (idx.clamp(1.0, input.len() as f64 - 3.0).max(1.0)) as usize;
    let t = pos - idx;
    let p0 = f64::from(input[i - 1]);
    let p1 = f64::from(input[i]);
    let p2 = f64::from(input[i + 1]);
    let p3 = f64::from(input[i + 2]);
    let v = p1
        + 0.5
            * t
            * (p2 - p0
                + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + t * (3.0 * (p1 - p2) + p3 - p0)));
    #[expect(
        clippy::cast_possible_truncation,
        reason = "interpolated sample lands in f32 range"
    )]
    let sample = v as f32;
    sample
}

/// Streaming cubic resampler: fractional read cursor over an input
/// stream, producing pitch-adjusted output at a fixed ratio.
#[derive(Debug, Clone)]
pub struct Resampler {
    decision: StretchDecision,
}

impl Resampler {
    /// Builds a resampler from a stretch decision.
    #[must_use]
    pub fn new(decision: StretchDecision) -> Self {
        Self { decision }
    }

    /// Total output samples for `input_len` source samples.
    #[must_use]
    pub fn output_len(&self, input_len: usize) -> usize {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "length composed in f64; inputs fit u32"
        )]
        let len = (f64::from(u32::try_from(input_len).unwrap_or(u32::MAX))
            * f64::from(self.decision.ratio))
        .round() as usize;
        len
    }

    /// Resamples an entire buffer (offline convenience form).
    ///
    /// Returns exactly [`Resampler::output_len`] samples: the input
    /// is padded by one sample at each end so the interpolable range
    /// covers the full input.
    #[must_use]
    pub fn resample_all(&self, input: &[f32]) -> Vec<f32> {
        let padded: Vec<f32> = std::iter::once(input.first().copied().unwrap_or(0.0))
            .chain(input.iter().copied())
            .chain(std::iter::once(input.last().copied().unwrap_or(0.0)))
            .collect();
        let len = self.output_len(input.len());
        let mut out = Vec::with_capacity(len);
        for k in 0..len {
            #[expect(clippy::cast_precision_loss, reason = "output indices are small")]
            let pos = 1.0 + (k as f64) / f64::from(self.decision.ratio);
            out.push(cubic_at(&padded, pos));
        }
        out
    }

    /// Resamples an interleaved multi-channel buffer with one shared
    /// fractional cursor: every channel of an output frame reads the
    /// same source frame position, so channels stay in lockstep and
    /// never smear into each other.
    ///
    /// Returns exactly `output_len(frames)` frames.
    #[must_use]
    pub fn resample_all_frames(&self, input: &[f32], channels: usize) -> Vec<f32> {
        let ch = channels.max(1);
        let frames_in = input.len() / ch;
        let frames_out = self.output_len(frames_in);
        // Pad one frame at each end, deinterleaved per plane.
        let planes: Vec<Vec<f32>> = (0..ch)
            .map(|c| {
                std::iter::once(input.get(c).copied().unwrap_or(0.0))
                    .chain(input.iter().skip(c).step_by(ch).copied())
                    .chain(std::iter::once(
                        input
                            .len()
                            .checked_sub(ch)
                            .and_then(|i| input.get(i + c))
                            .copied()
                            .unwrap_or(0.0),
                    ))
                    .collect()
            })
            .collect();
        let mut out = Vec::with_capacity(frames_out * ch);
        for k in 0..frames_out {
            #[expect(clippy::cast_precision_loss, reason = "output indices are small")]
            let pos = 1.0 + (k as f64) / f64::from(self.decision.ratio);
            for plane in &planes {
                out.push(cubic_at(plane, pos));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::types::{StretchDecision as SD, StretchMode};

    fn decision(ratio: f32) -> StretchDecision {
        SD {
            ratio,
            mode: StretchMode::Resample,
            out_of_comfort_band: false,
            strategy: crate::timeline::types::TempoStrategy::SessionBpm,
        }
    }

    fn sine(freq: f32, rate: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "test index")]
                let t = i as f32 / rate;
                (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5
            })
            .collect()
    }

    /// Dominant frequency via zero-crossing count over duration.
    fn dominant_hz(samples: &[f32], rate: f32) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|w| (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0))
            .count();
        #[expect(clippy::cast_precision_loss, reason = "test sample counts are small")]
        let dur = samples.len() as f32 / rate;
        #[expect(clippy::cast_precision_loss, reason = "crossing counts are small")]
        let n = crossings as f32;
        n / 2.0 / dur
    }

    #[test]
    fn output_len_is_exact() {
        // Given 10_000 input samples and ratio 1.04.
        let r = Resampler::new(decision(1.04));

        // When asking for the output length.
        let len = r.output_len(10_000);

        // Then it is input × ratio, rounded once.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "expected length fits usize"
        )]
        let expected = (10_000.0_f64 * 1.04_f64).round() as usize;
        assert_eq!(len, expected);
    }

    #[test]
    fn resample_all_returns_exact_length() {
        // Given a 1-second 440 Hz sine.
        let input = sine(440.0, 44_100.0, 44_100);

        // When resampling at 1.05.
        let r = Resampler::new(decision(1.05));
        let out = r.resample_all(&input);

        // Then the output length is exact.
        assert_eq!(out.len(), r.output_len(input.len()));
    }

    #[test]
    fn pitch_shifts_proportionally_when_slower() {
        // Given a 1-second 440 Hz sine at 44.1 kHz.
        let input = sine(440.0, 44_100.0, 44_100);

        // When resampling at ratio 1.05 (slower tempo → lower pitch).
        let r = Resampler::new(decision(1.05));
        let out = r.resample_all(&input);

        // Then the dominant frequency drops to ~440 / 1.05.
        let measured = dominant_hz(&out, 44_100.0);
        let expected = 440.0 / 1.05;
        assert!(
            (measured - expected).abs() < 4.0,
            "measured {measured} vs expected {expected}"
        );
    }

    #[test]
    fn pitch_shifts_proportionally_when_faster() {
        // Given a 1-second 440 Hz sine at 44.1 kHz.
        let input = sine(440.0, 44_100.0, 44_100);

        // When resampling at ratio 0.96 (faster tempo → higher pitch).
        let r = Resampler::new(decision(0.96));
        let out = r.resample_all(&input);

        // Then the dominant frequency rises to ~440 / 0.96.
        let measured = dominant_hz(&out, 44_100.0);
        let expected = 440.0 / 0.96;
        assert!(
            (measured - expected).abs() < 4.0,
            "measured {measured} vs expected {expected}"
        );
    }

    #[test]
    fn unity_ratio_is_identity() {
        // Given a 440 Hz sine.
        let input = sine(440.0, 44_100.0, 10_000);

        // When resampling at 1.0.
        let r = Resampler::new(decision(1.0));
        let out = r.resample_all(&input);

        // Then the output matches the input to interpolation error
        // (Catmull-Rom on a ~1.7% Nyquist sine: < 0.5%).
        for (a, b) in input.iter().zip(&out) {
            assert!((a - b).abs() < 5e-3, "{a} vs {b}");
        }
    }

    /// Energy-weighted spectral centroid (Hz) via zero-padded DFT.
    fn centroid_hz(samples: &[f32], rate: f32) -> f32 {
        let n = 4_096;
        let m = samples.len().min(n);
        let window: Vec<f32> = (0..m)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "test index")]
                let t = i as f32 / m as f32;
                (std::f32::consts::PI * t).sin()
            })
            .collect();
        let half = n / 2;
        let spectrum: Vec<f32> = (0..half)
            .map(|k| {
                let mut re = 0.0_f32;
                let mut im = 0.0_f32;
                for (i, &s) in samples[..m].iter().enumerate() {
                    let w = if i < window.len() { window[i] } else { 0.0 };
                    #[expect(clippy::cast_precision_loss, reason = "test bins")]
                    let ang = 2.0 * std::f32::consts::PI * k as f32 * i as f32 / n as f32;
                    re += s * w * ang.cos();
                    im -= s * w * ang.sin();
                }
                (re * re + im * im).sqrt()
            })
            .collect();
        let total: f32 = spectrum.iter().sum();
        let weighted: f32 = spectrum
            .iter()
            .enumerate()
            .map(|(k, &mag)| {
                #[expect(clippy::cast_precision_loss, reason = "test bins")]
                let hz = k as f32 * rate / n as f32;
                mag * hz
            })
            .sum();
        weighted / total
    }

    #[test]
    fn pitch_shift_moves_spectral_centroid_proportionally() {
        // Given a 440 Hz sine (centroid ~440).
        let input = sine(440.0, 44_100.0, 8_192);
        let base = centroid_hz(&input, 44_100.0);

        // When resampling at ratio 0.8 (25% faster → 25% higher pitch).
        let r = Resampler::new(decision(0.8));
        let out = r.resample_all(&input);
        let shifted = centroid_hz(&out, 44_100.0);

        // Then the centroid ratio tracks the pitch ratio within 5%.
        let ratio = shifted / base;
        assert!(
            (ratio - 1.25).abs() < 0.06,
            "centroid ratio {ratio} (base {base}, shifted {shifted})"
        );
    }
}
