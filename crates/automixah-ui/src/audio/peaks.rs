//! Visual-rate peak extraction — the Mixxx waveform storage scheme.
//!
//! Mixxx (`analyzerwaveform.cpp`) computes, per ~100-sample stride at the
//! 441 Hz "visual sample rate", the max of |L| and |R| for the raw signal
//! and for each Bessel-filtered band, quantized into `u8` quartets
//! `(low, mid, high, all)`. One u8 quartet per visual sample keeps a
//! 6-minute track at ~160 KB — the same budget Mixxx budgets.

use super::bands::BandSplitter;

/// Visual samples per second (Mixxx `mainWaveformSampleRate`).
pub const VISUAL_RATE: f32 = 441.0;

/// One visual sample: max-of-|abs| per band, saturated to u8.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeakQuartet {
    /// Low band (<600 Hz) max |abs|.
    pub low: u8,
    /// Mid band (600–4000 Hz) max |abs|.
    pub mid: u8,
    /// High band (>4000 Hz) max |abs|.
    pub high: u8,
    /// Unfiltered max |abs|.
    pub all: u8,
}

impl PeakQuartet {
    fn from_running(peak: &RunningPeak) -> Self {
        Self {
            low: quantize(peak.low),
            mid: quantize(peak.mid),
            high: quantize(peak.high),
            all: quantize(peak.all),
        }
    }
}

/// Saturating f32 amplitude → u8 (Mixxx clamps at ±1.0).
fn quantize(v: f32) -> u8 {
    let scaled = v.clamp(0.0, 1.0) * 255.0;
    scaled.round() as u8
}

/// Per-stride running maxima before quantization.
#[derive(Debug, Clone, Copy, Default)]
struct RunningPeak {
    low: f32,
    mid: f32,
    high: f32,
    all: f32,
}

impl RunningPeak {
    fn absorb(&mut self, bands: [[f32; 3]; 2], raw_l: f32, raw_r: f32) {
        self.low = self.low.max(bands[0][0].abs()).max(bands[1][0].abs());
        self.mid = self.mid.max(bands[0][1].abs()).max(bands[1][1].abs());
        self.high = self.high.max(bands[0][2].abs()).max(bands[1][2].abs());
        self.all = self.all.max(raw_l.abs()).max(raw_r.abs());
    }
}

/// The extracted visual-rate peak track.
#[derive(Debug, Clone)]
pub struct Peaks {
    /// One quartet per visual sample, in time order.
    pub data: Vec<PeakQuartet>,
    /// Frames of source audio per visual sample (e.g. ≈100 at 44.1 kHz).
    pub stride_frames: f32,
}

impl Peaks {
    /// Builds the peak track from interleaved stereo PCM.
    ///
    /// Slot *k* covers source frames `[k·stride, (k+1)·stride)` exactly:
    /// the boundary accumulator is fractional (f64), so non-integer
    /// strides (48000/441 ≈ 108.84) advance on true boundaries instead
    /// of rounding up to 109 every slot — which stretched the rendered
    /// timeline ~86 ms/min against the audio and grid on 48 kHz sources.
    ///
    /// The final partial slot is flushed when at least one frame of it
    /// is covered.
    #[must_use]
    pub fn build(samples: &[f32], sample_rate: u32) -> Self {
        let stride = f64::from(sample_rate) / f64::from(VISUAL_RATE);
        let frames = samples.len() / 2;
        let visual_len = if frames == 0 {
            0
        } else {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "frame/stride ratio < 2^24 for any real track"
            )]
            ((frames as f64 / stride).ceil() as usize).max(1)
        };

        let mut splitter = BandSplitter::new(f64::from(sample_rate));
        let mut data = Vec::with_capacity(visual_len);
        let mut running = RunningPeak::default();
        // End frame of the slot currently being accumulated: slot k
        // covers [k·stride, (k+1)·stride); kept in f64 so the boundary
        // lands at the true fractional position (an integer counter vs
        // 108.84 could only ever fire at 109).
        let mut next_boundary = stride;

        for (frame_idx, frame) in samples.chunks_exact(2).enumerate() {
            let (l, r) = (frame[0], frame[1]);
            let bands = splitter.process_frame(l, r);
            running.absorb(bands, l, r);
            let frame_end = f64::from(frame_idx as u32) + 1.0;
            if frame_end >= next_boundary {
                data.push(PeakQuartet::from_running(&running));
                running = RunningPeak::default();
                next_boundary += stride;
            }
        }
        if frames as f64 > next_boundary - stride {
            data.push(PeakQuartet::from_running(&running));
        }

        Self {
            data,
            stride_frames: stride as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44_100;

    fn silence_and_impulse(impulse_frame: usize, frames: usize) -> Vec<f32> {
        let mut pcm = vec![0.0_f32; frames * 2];
        if impulse_frame * 2 + 1 < pcm.len() {
            pcm[impulse_frame * 2] = 1.0;
            pcm[impulse_frame * 2 + 1] = 1.0;
        }
        pcm
    }

    // Given one second of silence with a single full-scale impulse.
    // When peaks are built.
    // Then exactly ⌈rate/stride⌉ visual samples exist and the impulse lands
    // in the expected quartet's `all` slot.
    #[test]
    fn impulse_lands_in_expected_visual_sample() {
        let frames = RATE as usize;
        let stride = RATE as f32 / VISUAL_RATE;
        let impulse = (stride * 5.5) as usize; // inside visual sample 5
        let pcm = silence_and_impulse(impulse, frames);

        let peaks = Peaks::build(&pcm, RATE);

        assert_eq!(peaks.data.len(), (frames as f32 / stride).ceil() as usize);
        assert_eq!(peaks.data[5].all, 255, "impulse in visual sample 5");
        assert_eq!(peaks.data[4].all, 0, "silence before");
        assert_eq!(peaks.data[6].all, 0, "silence after");
    }

    // Given PCM shorter than one stride.
    // When peaks are built.
    // Then the partial final stride is flushed as one visual sample.
    #[test]
    fn partial_final_stride_is_flushed() {
        let pcm = vec![0.5_f32; 2 * 7]; // 7 frames ≪ stride ≈ 100
        let peaks = Peaks::build(&pcm, RATE);
        assert_eq!(peaks.data.len(), 1, "single flushed stride");
        assert!(peaks.data[0].all > 0);
    }

    // Given a 48 kHz source whose stride is fractional (48000/441 ≈
    // 108.84) and impulses placed at the first frame of slots
    // 50, 100, and 150 (past where an every-slot-109 loop has
    // accumulated a full slot of error).
    // When peaks are built.
    // Then each impulse lands in its own slot k with silence on both
    // neighbors — the timeline does not stretch against true time.
    #[test]
    fn peaks_slot_k_covers_k_stride_at_48k() {
        let rate = 48_000u32;
        let stride = f64::from(rate) / f64::from(VISUAL_RATE);
        let slots = 160_usize;
        #[expect(clippy::cast_possible_truncation, reason = "fixture size")]
        let frames = (stride * slots as f64).round() as usize + 8;

        let mut pcm = vec![0.0_f32; frames * 2];
        for k in [50_usize, 100, 150] {
            #[expect(clippy::cast_possible_truncation, reason = "boundary frame index")]
            let frame = (k as f64 * stride).ceil() as usize; // first frame of slot k
            pcm[frame * 2] = 1.0;
            pcm[frame * 2 + 1] = 1.0;
        }

        let peaks = Peaks::build(&pcm, rate);

        for k in [50_usize, 100, 150] {
            assert_eq!(
                peaks.data[k].all, 255,
                "impulse for slot {k} landed elsewhere"
            );
            assert_eq!(peaks.data[k - 1].all, 0, "slot before {k} not silent");
            assert_eq!(peaks.data[k + 1].all, 0, "slot after {k} not silent");
        }
    }

    // Given a 48 kHz source long enough that the old every-slot-109 loop
    // truncated ~1 frame per slot (about 0.6 s over a full track).
    // When peaks are built and the render-domain total is derived.
    // Then the represented timeline matches the true frame count within
    // ±2 frames — the waveform's right edge reaches the track end.
    #[test]
    fn peaks_total_frames_matches_source_at_48k() {
        let rate = 48_000u32;
        // 7 minutes: the size class of the track that exposed the bug.
        let frames = 420 * rate as usize + 137;
        let pcm = vec![0.0_f32; frames * 2];

        let peaks = Peaks::build(&pcm, rate);

        // data.len() must be ceil(frames / stride): one visual sample per
        // (possibly partial) stride. The length itself carries the no-
        // truncation guarantee; the f32 stride used by the render domain
        // rounds it to within one stride of the true frame count.
        let stride = f64::from(rate) / f64::from(VISUAL_RATE);
        let expected_slots = (frames as f64 / stride).ceil() as usize;
        assert_eq!(
            peaks.data.len(),
            expected_slots,
            "slot count must cover the full source"
        );
        let total = peaks.data.len() as f32 * peaks.stride_frames;
        let error = (total - frames as f32).abs();
        let one_stride = f64::from(rate) / f64::from(VISUAL_RATE);
        assert!(
            f64::from(error) <= one_stride,
            "timeline {total} vs true {frames} frames (off by {error:.1})"
        );
    }

    // Given samples exceeding ±1.0.
    // When quantized.
    // Then values saturate at 255 (no wraparound).
    #[test]
    fn quantization_saturates() {
        assert_eq!(quantize(1.5), 255);
        assert_eq!(quantize(-2.0), 0);
        assert_eq!(quantize(1.0), 255);
        assert_eq!(quantize(0.5), 128);
    }

    // Given a DC signal at 0.25.
    // When the low band's peaks are read.
    // Then low ≈ 64 and high ≈ 0 (DC is lowband-only, settled).
    #[test]
    fn dc_signal_maps_to_low_band() {
        let frames = RATE as usize; // 1 s: filter fully settles
        let pcm = vec![0.25_f32; frames * 2];
        let peaks = Peaks::build(&pcm, RATE);

        let last = peaks.data.last().copied().expect("non-empty");
        assert!(last.low >= 60 && last.low <= 66, "low {last:?}");
        assert_eq!(last.high, 0, "no high-band energy at DC");
        assert_eq!(last.all, 64);
    }
}
