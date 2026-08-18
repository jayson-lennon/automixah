//! WSOLA time-stretching (the pitch-preserving path).
//!
//! Waveform-Similarity Overlap-Add: frames of ~1536 samples are
//! overlap-added at a fixed synthesis hop (`frame/2`); the analysis
//! read position is chosen within a ±10 ms search window to maximize
//! normalized cross-correlation with the natural continuation
//! already in the output buffer. Pitch is preserved because the
//! waveform content itself is not resampled — only its placement.
//!
//! Output length is **exact** by construction: the output buffer is
//! `round(input_len × ratio)` samples, frames are added until the
//! buffer is full, and a normalization pass divides by the summed
//! Hann envelope.

use crate::render::resample::Resampler;
use crate::timeline::types::StretchDecision;

/// WSOLA frame length in samples (~35 ms at 44.1 kHz).
pub const FRAME: usize = 1536;

/// Synthesis hop: half the frame (50% Hann overlap).
pub const SYNTH_HOP: usize = FRAME / 2;

/// Correlation search half-width at 44.1 kHz (~10 ms).
pub const SEARCH: usize = 441;

/// Pitch-preserving stretcher at a fixed duration-scale ratio.
#[derive(Debug, Clone)]
pub struct Wsola {
    decision: StretchDecision,
}

impl Wsola {
    /// Builds a stretcher from a stretch decision.
    #[must_use]
    pub fn new(decision: StretchDecision) -> Self {
        Self { decision }
    }

    /// Stretches an entire buffer. Returns exactly
    /// `round(len × ratio)` samples. Inputs shorter than one frame
    /// plus search fall back to the (pitch-adjusted) resampler —
    /// under ~40 ms of audio, pitch artifacts are irrelevant.
    #[must_use]
    pub fn stretch_all(&self, input: &[f32]) -> Vec<f32> {
        let ratio = f64::from(self.decision.ratio);
        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "output length composed in f64, fits usize"
        )]
        let out_len = ((input.len() as f64) * ratio).round() as usize;

        if input.len() < FRAME + SEARCH {
            return Resampler::new(self.decision).resample_all(input);
        }

        #[expect(clippy::cast_precision_loss, reason = "hop constant fits f64 exactly")]
        let analysis_hop = (SYNTH_HOP as f64) / ratio;
        let hann = hann_window(FRAME);

        let mut out = vec![0.0_f32; out_len];
        let mut norm = vec![0.0_f32; out_len];

        let steps = out_len.div_ceil(SYNTH_HOP);
        for k in 0..steps {
            #[expect(clippy::cast_precision_loss, reason = "step index small")]
            let ideal_f = (k as f64) * analysis_hop;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "read position bounded by clamped search"
            )]
            let ideal = ideal_f.round() as usize;

            // The template is what the previous frame left at this
            // synthesis position; correlate the candidate reads
            // against it to find the most similar continuation.
            let out_pos = (k * SYNTH_HOP).min(out_len);
            let region = (out_len - out_pos).min(SYNTH_HOP);
            let offset: isize = if k == 0 || region == 0 {
                0
            } else {
                best_offset(input, &out[out_pos..out_pos + region], ideal)
            };

            let src = ideal.saturating_add_signed(offset);
            add_frame(&mut out, &mut norm, &hann, input, src, out_pos);
        }

        normalize(&mut out, &norm);
        out
    }

    /// Frame-aware stretch of interleaved multi-channel audio:
    /// correlation runs on the mono mixdown to pick read offsets,
    /// then whole frames are copied so channels stay aligned and
    /// independent. Returns exactly `round(frames × ratio)` frames.
    #[must_use]
    pub fn stretch_all_frames(&self, input: &[f32], channels: usize) -> Vec<f32> {
        let ch = channels.max(1);
        let frames_in = input.len() / ch;
        if frames_in < FRAME + SEARCH || ch == 1 {
            return self.stretch_all(input);
        }

        // Plan the synthesis on the mono mixdown (offset decisions),
        // then apply the same offsets to every channel's plane.
        let mix: Vec<f32> = input
            .chunks(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32)
            .collect();
        let offsets = plan_offsets(&mix, f64::from(self.decision.ratio));

        let planes: Vec<Vec<f32>> = (0..ch)
            .map(|c| input.iter().skip(c).step_by(ch).copied().collect())
            .collect();
        let mut out_planes: Vec<Vec<f32>> = planes
            .iter()
            .map(|p| {
                let mut o = vec![0.0_f32; offsets.out_len];
                let mut n = vec![0.0_f32; offsets.out_len];
                let hann = hann_window(FRAME);
                for (k, &off) in offsets.reads.iter().enumerate() {
                    let dst = (k * SYNTH_HOP).min(offsets.out_len);
                    add_frame(&mut o, &mut n, &hann, p, off, dst);
                }
                normalize(&mut o, &n);
                o
            })
            .collect();

        let mut out = Vec::with_capacity(offsets.out_len * ch);
        for f in 0..offsets.out_len {
            for p in &mut out_planes {
                out.push(p[f]);
            }
        }
        out
    }
}

/// WSOLA read-offset plan computed on one representative signal.
struct OffsetPlan {
    /// Chosen input read position per synthesis step.
    reads: Vec<usize>,
    /// Total output length in frames.
    out_len: usize,
}

/// Runs the WSOLA offset search without producing audio: the same
/// stepping and correlation as [`Wsola::stretch_all`], recording
/// each step's chosen read.
fn plan_offsets(input: &[f32], ratio: f64) -> OffsetPlan {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "output length composed in f64, fits usize"
    )]
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    #[expect(clippy::cast_precision_loss, reason = "hop constant fits f64 exactly")]
    let analysis_hop = (SYNTH_HOP as f64) / ratio;

    let steps = out_len.div_ceil(SYNTH_HOP);
    let mut reads = Vec::with_capacity(steps);
    let mut out_tail = vec![0.0_f32; out_len];
    let mut norm = vec![0.0_f32; out_len];
    let hann = hann_window(FRAME);

    for k in 0..steps {
        #[expect(clippy::cast_precision_loss, reason = "step index small")]
        let ideal_f = (k as f64) * analysis_hop;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "read position bounded by clamped search"
        )]
        let ideal = ideal_f.round() as usize;
        let out_pos = (k * SYNTH_HOP).min(out_len);
        let region = (out_len - out_pos).min(SYNTH_HOP);
        let offset: isize = if k == 0 || region == 0 {
            0
        } else {
            best_offset(input, &out_tail[out_pos..out_pos + region], ideal)
        };
        let src = ideal.saturating_add_signed(offset);
        reads.push(src);
        add_frame(&mut out_tail, &mut norm, &hann, input, src, out_pos);
    }
    OffsetPlan { reads, out_len }
}

/// The Hann window of length `n` (periodic form).
fn hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "window indices are small")]
            let t = i as f32 / n as f32;
            (std::f32::consts::PI * t).sin().powi(2)
        })
        .collect()
}

/// Searches ±[`SEARCH`] around `ideal` for the input read whose
/// first `template.len()` samples best match `template` (NCCF).
fn best_offset(input: &[f32], template: &[f32], ideal: usize) -> isize {
    let lo = ideal.saturating_sub(SEARCH).max(1);
    let hi = (ideal + SEARCH).min(input.len().saturating_sub(FRAME + 1));
    if lo > hi {
        return 0;
    }
    let ideal_cand = ideal.clamp(lo, hi);

    let mut best = ideal_cand;
    let mut best_score = f64::NEG_INFINITY;
    for cand in lo..=hi {
        let a = &input[cand..cand + template.len()];
        let score = nccf(a, template);
        if score > best_score {
            best_score = score;
            best = cand;
        }
    }
    #[expect(
        clippy::cast_possible_wrap,
        reason = "offset within ±SEARCH around a bounded ideal"
    )]
    let delta = best as isize - ideal_cand as isize;
    delta
}

/// Normalized cross-correlation of two equal-length slices.
fn nccf(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut ea, mut eb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        ea += x * x;
        eb += y * y;
    }
    dot / (ea * eb).sqrt().max(1e-12)
}

/// Adds one windowed frame of `input` at `src` into `out` at `dst`.
fn add_frame(
    out: &mut [f32],
    norm: &mut [f32],
    hann: &[f32],
    input: &[f32],
    src: usize,
    dst: usize,
) {
    let src = src.min(input.len().saturating_sub(FRAME));
    for j in 0..FRAME.min(out.len() - dst) {
        out[dst + j] += input[src + j] * hann[j];
        norm[dst + j] += hann[j];
    }
}

/// Divides by the summed window envelope (floored at a small value).
fn normalize(out: &mut [f32], norm: &[f32]) {
    for (o, &n) in out.iter_mut().zip(norm) {
        *o /= n.max(1e-6);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::types::{StretchDecision as SD, StretchMode};

    fn decision(ratio: f32) -> StretchDecision {
        SD {
            ratio,
            mode: StretchMode::Wsola,
            out_of_comfort_band: true,
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

    fn dominant_hz(samples: &[f32], rate: f32) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|w| (w[0] < 0.0 && w[1] >= 0.0) || (w[0] >= 0.0 && w[1] < 0.0))
            .count();
        #[expect(clippy::cast_precision_loss, reason = "test counts are small")]
        let dur = samples.len() as f32 / rate;
        #[expect(clippy::cast_precision_loss, reason = "crossing counts are small")]
        let n = crossings as f32;
        n / 2.0 / dur
    }

    #[test]
    fn stretch_up_yields_exact_longer_duration() {
        // Given 2 seconds of 440 Hz.
        let input = sine(440.0, 44_100.0, 88_200);

        // When stretching by 1.25.
        let out = Wsola::new(decision(1.25)).stretch_all(&input);

        // Then the output is exactly 1.25× longer.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test length fits usize"
        )]
        let expected = (88_200.0_f64 * 1.25).round() as usize;
        assert_eq!(out.len(), expected);
    }

    #[test]
    fn stretch_down_yields_exact_shorter_duration() {
        // Given 2 seconds of 440 Hz.
        let input = sine(440.0, 44_100.0, 88_200);

        // When stretching by 0.8.
        let out = Wsola::new(decision(0.8)).stretch_all(&input);

        // Then the output is exactly 0.8× as long.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test length fits usize"
        )]
        let expected = (88_200.0_f64 * 0.8).round() as usize;
        assert_eq!(out.len(), expected);
    }

    #[test]
    fn stretch_preserves_pitch_when_longer() {
        // Given 2 seconds of 440 Hz.
        let input = sine(440.0, 44_100.0, 88_200);

        // When stretching by 1.25 (25% longer).
        let out = Wsola::new(decision(1.25)).stretch_all(&input);

        // Then the dominant frequency is still ~440 Hz.
        let measured = dominant_hz(&out[SEARCH..out.len() - SEARCH], 44_100.0);
        assert!((measured - 440.0).abs() < 4.0, "measured {measured} vs 440");
    }

    #[test]
    fn stretch_preserves_pitch_when_shorter() {
        // Given 2 seconds of 440 Hz.
        let input = sine(440.0, 44_100.0, 88_200);

        // When stretching by 0.8 (20% shorter).
        let out = Wsola::new(decision(0.8)).stretch_all(&input);

        // Then the dominant frequency is still ~440 Hz.
        let measured = dominant_hz(&out[SEARCH..out.len() - SEARCH], 44_100.0);
        assert!((measured - 440.0).abs() < 4.0, "measured {measured} vs 440");
    }

    #[test]
    fn output_is_finite() {
        // Given a varied signal (two sines).
        let a = sine(220.0, 44_100.0, 88_200);
        let b = sine(277.0, 44_100.0, 88_200);
        let input: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();

        // When stretching.
        let out = Wsola::new(decision(1.12)).stretch_all(&input);

        // Then every sample is finite.
        assert!(out.iter().all(|x| x.is_finite()));
    }
}
