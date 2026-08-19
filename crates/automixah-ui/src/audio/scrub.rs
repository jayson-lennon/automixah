//! Vinyl-style varispeed scrub reader.
//!
//! Reads a mono/stereo buffer at a fractional, time-varying position with
//! cubic-Hermite interpolation. Pitch follows speed (vinyl behavior): at 0.5×
//! the tone drops an octave, at 2× it rises one. On speed changes the
//! reader crossfades between old and new step over `CROSSFADE` output frames
//! so drag velocity changes never click.

/// Maximum |speed| (forward or reverse scrub).
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "wired to cpal in the next phase-4 task")
)]
pub const MAX_SPEED: f32 = 8.0;
/// Output frames over which a speed change ramps.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "wired to cpal in the next phase-4 task")
)]
pub const CROSSFADE: f32 = 64.0;

/// Varispeed reader over interleaved stereo PCM.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "wired to cpal in the next phase-4 task")
)]
pub struct ScrubCore {
    /// Channel count of the source buffer.
    channels: usize,
    /// Current fractional read position in frames.
    ///
    /// f64, not f32: an f32 frame position freezes past 2²⁴ frames
    /// (~6.3 min at 44.1 kHz) because `pos + 1.0` rounds back to `pos`,
    /// so long tracks stop playing near their end.
    position: f64,
    /// Current step per output frame (target speed).
    step: f32,
    /// Step being faded out (previous speed).
    prev_step: f32,
    /// Frames remaining in the speed crossfade.
    fade_remaining: f32,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "wired to cpal in the next phase-4 task")
)]
impl ScrubCore {
    /// Creates a reader parked at `start_frame`.
    #[must_use]
    pub fn new(channels: usize, start_frame: f64) -> Self {
        Self {
            channels: channels.max(1),
            position: start_frame,
            step: 1.0,
            prev_step: 1.0,
            fade_remaining: 0.0,
        }
    }

    /// Current read position in frames (for playhead sync).
    #[must_use]
    pub fn position(&self) -> f64 {
        self.position
    }

    /// Sets the speed (frames of source per output frame), clamped and
    /// crossfaded from the previous speed.
    pub fn set_speed(&mut self, speed: f32) {
        let target = speed.clamp(-MAX_SPEED, MAX_SPEED);
        if (target - self.step).abs() < f32::EPSILON {
            return;
        }
        self.prev_step = self.effective_step();
        self.step = target;
        self.fade_remaining = CROSSFADE;
    }

    /// Instantaneous blended step (mid-crossfade).
    #[must_use]
    pub fn effective_step(&self) -> f32 {
        if self.fade_remaining <= 0.0 {
            self.step
        } else {
            let t = 1.0 - self.fade_remaining / CROSSFADE;
            self.prev_step + (self.step - self.prev_step) * t
        }
    }

    /// Renders `out.len()` interleaved frames, advancing the position.
    ///
    /// Beyond the buffer ends the reader emits silence and the position
    /// clamps (does not run away).
    pub fn read(&mut self, samples: &[f32], out: &mut [f32]) {
        let channels = self.channels;
        let frames = samples.len() / channels;
        #[expect(clippy::cast_precision_loss, reason = "frame index fits f64 exactly")]
        let last = frames.saturating_sub(1) as f64;

        for chunk in out.chunks_mut(channels) {
            let step = self.effective_step();
            if self.fade_remaining > 0.0 {
                self.fade_remaining -= 1.0;
            }

            let pos = self.position;
            if pos <= 0.0 && step <= 0.0 || pos >= last && step >= 0.0 {
                // Clamped at an end: silence, hold position.
                chunk.fill(0.0);
                self.position = pos.clamp(0.0, last);
                continue;
            }

            for (ch, o) in chunk.iter_mut().enumerate() {
                *o = hermite(samples, channels, ch, pos);
            }
            self.position = (pos + f64::from(step)).clamp(0.0, last);
        }
    }
}

/// Four-point cubic-Hermite interpolation of `channel` at fractional frame
/// `pos`, with flat extrapolation at the edges.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "called via read in the cpal task")
)]
fn hermite(samples: &[f32], channels: usize, ch: usize, pos: f64) -> f32 {
    let frames = samples.len() / channels;
    if frames == 0 {
        return 0.0;
    }
    // The integer index comes from the f64 floor directly: at f32 the
    // integer part consumes all 24 mantissa bits past 2²⁴ frames, and the
    // fractional part (the interpolation) would vanish.
    let floor = pos.floor();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "floored index within bounds"
    )]
    let i = (floor as usize).min(frames - 1);
    let t = (pos - floor) as f32;
    let at = |frame: isize| -> f32 {
        let idx = frame.clamp(0, frames as isize - 1) as usize;
        samples[idx * channels + ch]
    };
    let y0 = at(i as isize - 1);
    let y1 = at(i as isize);
    let y2 = at(i as isize + 1);
    let y3 = at(i as isize + 2);
    // Catmull-Rom tangents.
    let m1 = (y2 - y0) * 0.5;
    let m2 = (y3 - y1) * 0.5;
    let t2 = t * t;
    let t3 = t2 * t;
    (2.0 * t3 - 3.0 * t2 + 1.0) * y1
        + (t3 - 2.0 * t2 + t) * m1
        + (-2.0 * t3 + 3.0 * t2) * y2
        + (t3 - t2) * m2
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const RATE: f32 = 44_100.0;

    fn sine_sine(hz: f32, seconds: f32) -> Vec<f32> {
        let frames = (RATE * seconds) as usize;
        (0..frames)
            .flat_map(|i| {
                let v = (TAU * hz * i as f32 / RATE).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    fn dominant_hz(out: &[f32], rate: f32) -> f32 {
        // Zero crossings → average period.
        let mono: Vec<f32> = out.iter().step_by(2).copied().collect();
        let mut crossings = 0usize;
        let mut first = None;
        let mut last = None;
        for w in mono.windows(2) {
            if w[0] < 0.0 && w[1] >= 0.0 {
                crossings += 1;
                if first.is_none() {
                    first = Some(crossings);
                }
                last = Some(crossings);
            }
        }
        let (Some(f), Some(l)) = (first, last) else {
            return 0.0;
        };
        if l <= f {
            return 0.0;
        }
        rate * (l - f) as f32 / mono.len() as f32
    }

    // Given a 440 Hz sine read at 1x.
    // When 1 s is rendered.
    // Then the output is ~440 Hz (identity).
    #[test]
    fn one_x_preserves_frequency() {
        let src = sine_sine(440.0, 3.0);
        let mut core = ScrubCore::new(2, f64::from(RATE));
        core.set_speed(1.0);
        let mut out = vec![0.0_f32; (RATE * 1.0) as usize * 2];
        core.read(&src, &mut out);
        let hz = dominant_hz(&out[2205 * 4..], RATE);
        assert!((hz - 440.0).abs() < 8.0, "measured {hz} Hz");
    }

    // Given a 440 Hz sine.
    // When read at 0.5x.
    // Then the output is ~220 Hz (pitch follows speed downward).
    #[test]
    fn half_x_drops_an_octave() {
        let src = sine_sine(440.0, 4.0);
        let mut core = ScrubCore::new(2, f64::from(RATE));
        core.set_speed(0.5);
        let mut out = vec![0.0_f32; (RATE * 1.0) as usize * 2];
        core.read(&src, &mut out);
        let hz = dominant_hz(&out[2205 * 4..], RATE);
        assert!((hz - 220.0).abs() < 8.0, "measured {hz} Hz");
    }

    // Given a 440 Hz sine.
    // When read at 2x.
    // Then the output is ~880 Hz (pitch follows speed upward).
    #[test]
    fn two_x_rises_an_octave() {
        let src = sine_sine(440.0, 3.0);
        let mut core = ScrubCore::new(2, f64::from(RATE));
        core.set_speed(2.0);
        let mut out = vec![0.0_f32; (RATE * 1.0) as usize * 2];
        core.read(&src, &mut out);
        let hz = dominant_hz(&out[2205 * 4..], RATE);
        assert!((hz - 880.0).abs() < 16.0, "measured {hz} Hz");
    }

    // Given a speed change mid-read.
    // When the output is inspected.
    // Then the step blends over CROSSFADE frames (no discontinuity in
    // effective step: each frame moves by at most the larger step).
    #[test]
    fn speed_change_crossfades() {
        let src = sine_sine(440.0, 5.0);
        let mut core = ScrubCore::new(2, f64::from(RATE));
        core.set_speed(1.0);
        core.fade_remaining = 0.0;
        // Jump 1x -> 3x.
        core.set_speed(3.0);
        assert_eq!(core.fade_remaining, CROSSFADE);
        // Consume one frame; effective_step must have moved toward 3.
        let mut out = vec![0.0_f32; 2];
        core.read(&src, &mut out);
        let blended = core.effective_step();
        assert!(blended > 1.0 && blended < 3.0, "blended step {blended}");
        let mut out = vec![0.0_f32; (CROSSFADE as usize) * 2];
        core.read(&src, &mut out);
        assert_eq!(core.fade_remaining, 0.0, "fade exhausted");
        assert!((core.effective_step() - 3.0).abs() < 1e-6);
    }

    // Given a reader parked at 2²⁴ frames (16,777,216) — the exact f32
    // integer limit where an f32 position would freeze (`pos + 1.0 == pos`)
    // — over a mono buffer long enough to span that frame (zeroed vec:
    // lazily-mapped pages, only a few are ever touched).
    // When one frame is read at 1×.
    // Then the position advances by exactly one frame (integer-exact).
    #[test]
    fn position_advances_exactly_past_2p24_frames() {
        let src = vec![0.0_f32; 16_777_218];
        let mut core = ScrubCore::new(1, 16_777_216.0);
        core.set_speed(1.0);
        core.fade_remaining = 0.0;
        let mut out = vec![0.0_f32; 1];
        core.read(&src, &mut out);
        assert_eq!(core.position(), 16_777_217.0, "position must not freeze");
    }

    // Given a reader at the buffer end.
    // When reading forward.
    // Then output is silence and the position stays clamped.
    #[test]
    fn end_of_track_clamps() {
        let src = sine_sine(440.0, 0.1);
        let frames = src.len() / 2;
        let mut core = ScrubCore::new(2, frames as f64 - 0.5);
        core.set_speed(1.0);
        let mut out = vec![0.0_f32; 256 * 2];
        core.read(&src, &mut out);
        assert!(out.iter().all(|&v| v.abs() < 1e-6), "silence at end");
        assert!(core.position() <= (frames - 1) as f64 + 1e-3);
    }

    // Given a requested speed of 100.
    // When set.
    // Then it clamps to MAX_SPEED.
    #[test]
    fn speed_clamps_to_range() {
        let mut core = ScrubCore::new(2, 0.0);
        core.set_speed(100.0);
        assert_eq!(core.step, MAX_SPEED);
        core.set_speed(-100.0);
        assert_eq!(core.step, -MAX_SPEED);
    }
}
