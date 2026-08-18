//! Per-deck DSP: EQ, filters, and parameter smoothing.
//!
//! Each deck processes PCM through:
//!
//! 1. gain (from the control bus, smoothed),
//! 2. a 3-band EQ — low/high shelves and a mid peak (all biquads),
//! 3. a 2×2 cascade of HPF/LPF Butterworth biquads (the
//!    `HpfCutoff`/`LpfCutoff` automation surface),
//!
//! with all parameters smoothed by a one-pole filter (~10 ms) in
//! 64-sample blocks so curves don't zipper.

use crate::control::{ControlBus, DeckId, ParamAddress};

/// Processing block size in samples.
pub const BLOCK: usize = 64;

/// Parameter smoothing time constant (~10 ms at 44.1 kHz).
const SMOOTH_SAMPLES: f32 = 441.0;

/// A direct-form-1 biquad section.
#[derive(Debug, Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Identity filter (passes signal unchanged).
    #[must_use]
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            ..Self::default()
        }
    }

    /// Low shelf at `hz` with gain `db` (RBJ cookbook).
    #[must_use]
    pub fn low_shelf(hz: f32, db: f32, rate: f32) -> Self {
        let (w0, alpha) = rbj_shelf(hz, rate);
        let a = 10.0_f32.powf(db / 40.0);
        let cw = w0.cos();
        let sq = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) + (a - 1.0) * cw + sq;
        Self {
            b0: (a * ((a + 1.0) - (a - 1.0) * cw + sq)) / a0,
            b1: (2.0 * a * ((a - 1.0) - (a + 1.0) * cw)) / a0,
            b2: (a * ((a + 1.0) - (a - 1.0) * cw - sq)) / a0,
            a1: (-2.0 * ((a - 1.0) + (a + 1.0) * cw)) / a0,
            a2: ((a + 1.0) + (a - 1.0) * cw - sq) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// High shelf at `hz` with gain `db` (RBJ cookbook).
    #[must_use]
    pub fn high_shelf(hz: f32, db: f32, rate: f32) -> Self {
        let (w0, alpha) = rbj_shelf(hz, rate);
        let a = 10.0_f32.powf(db / 40.0);
        let cw = w0.cos();
        let sq = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) + (a - 1.0) * cw + sq;
        Self {
            b0: (a * ((a + 1.0) + (a - 1.0) * cw + sq)) / a0,
            b1: (-2.0 * a * ((a - 1.0) + (a + 1.0) * cw)) / a0,
            b2: (a * ((a + 1.0) + (a - 1.0) * cw - sq)) / a0,
            a1: (2.0 * ((a - 1.0) - (a + 1.0) * cw)) / a0,
            a2: ((a + 1.0) - (a - 1.0) * cw - sq) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Peaking EQ at `hz`, Q `q`, gain `db` (RBJ cookbook).
    #[must_use]
    pub fn peaking(hz: f32, q: f32, db: f32, rate: f32) -> Self {
        let (w0, alpha, a) = rbj_common_q(hz, q, db, rate);
        let cw = w0.cos();
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * cw) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * cw) / a0,
            a2: (1.0 - alpha / a) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Adopts `other`'s coefficients, keeping this filter's state
    /// (used for smoothed cutoff retunes).
    fn retune(&mut self, other: &Self) {
        self.b0 = other.b0;
        self.b1 = other.b1;
        self.b2 = other.b2;
        self.a1 = other.a1;
        self.a2 = other.a2;
    }

    /// Processes one sample.
    pub fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Processes a buffer in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        for x in buf.iter_mut() {
            *x = self.tick(*x);
        }
    }
}

/// RBJ shelf terms: plain alpha (the `2·sqrt(A)·alpha` term is
/// applied by the callers per the cookbook).
fn rbj_shelf(hz: f32, rate: f32) -> (f32, f32) {
    let w0 = 2.0 * std::f32::consts::PI * hz / rate;
    let alpha = w0.sin() / 2.0; // Q = 0.707 family
    (w0, alpha)
}

/// RBJ common terms with an explicit Q (peaking).
fn rbj_common_q(hz: f32, q: f32, db: f32, rate: f32) -> (f32, f32, f32) {
    let w0 = 2.0 * std::f32::consts::PI * hz / rate;
    let alpha = w0.sin() / (2.0 * q.max(0.1));
    let a = 10.0_f32.powf(db / 40.0);
    (w0, alpha, a)
}

/// One-pole parameter smoother (per smoothed parameter).
#[derive(Debug, Clone)]
pub struct Smoother {
    target: f32,
    current: f32,
    alpha: f32,
}

impl Smoother {
    /// Builds a smoother reaching ~63% of a step per `tau` samples.
    #[must_use]
    pub fn new(initial: f32, tau: f32) -> Self {
        Self {
            target: initial,
            current: initial,
            alpha: 1.0 - (-1.0 / tau.max(1.0)).exp(),
        }
    }

    /// Sets the target value.
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Advances the smoother one sample; returns the current value.
    pub fn tick(&mut self) -> f32 {
        self.current += self.alpha * (self.target - self.current);
        self.current
    }
}

/// Cutoff-mapping constants for the normalized filter params.
const HPF_MIN_HZ: f32 = 20.0;
const HPF_MAX_HZ: f32 = 2_000.0;
const LPF_MIN_HZ: f32 = 1_000.0;
const LPF_MAX_HZ: f32 = 20_000.0;

/// Maps a normalized `[0, 1]` HPF param to Hz (exponential).
#[must_use]
pub fn hpf_hz(norm: f32) -> f32 {
    HPF_MIN_HZ * (HPF_MAX_HZ / HPF_MIN_HZ).powf(norm.clamp(0.0, 1.0))
}

/// Maps a normalized `[0, 1]` LPF param to Hz (exponential).
#[must_use]
pub fn lpf_hz(norm: f32) -> f32 {
    LPF_MIN_HZ * (LPF_MAX_HZ / LPF_MIN_HZ).powf(norm.clamp(0.0, 1.0))
}

/// Maps a normalized `[0, 1]` EQ param to dB (±12 dB, 0.5 = unity).
#[must_use]
pub fn eq_db(norm: f32) -> f32 {
    (norm.clamp(0.0, 1.0) - 0.5) * 24.0
}

/// One deck's processing chain.
#[derive(Debug, Clone)]
pub struct DeckChain {
    eq_low: [Biquad; 2],
    eq_mid: [Biquad; 2],
    eq_high: [Biquad; 2],
    hpf: [[Biquad; 2]; 2],
    lpf: [[Biquad; 2]; 2],
    hpf_norm: f32,
    lpf_norm: f32,
    gain: Smoother,
    cutoffs: [Smoother; 2],
    rate: f32,
}

impl DeckChain {
    /// Builds a unity deck at `rate`.
    #[must_use]
    pub fn new(rate: f32, initial_gain: f32) -> Self {
        Self {
            eq_low: [Biquad::low_shelf(200.0, 0.0, rate); 2],
            eq_mid: [Biquad::peaking(1_000.0, 1.0, 0.0, rate); 2],
            eq_high: [Biquad::high_shelf(4_000.0, 0.0, rate); 2],
            hpf: [[Biquad::identity(), Biquad::identity()]; 2],
            hpf_norm: 0.0,
            lpf: [[Biquad::identity(), Biquad::identity()]; 2],
            lpf_norm: 1.0,
            gain: Smoother::new(initial_gain, SMOOTH_SAMPLES),
            cutoffs: [
                Smoother::new(0.0, SMOOTH_SAMPLES),
                Smoother::new(1.0, SMOOTH_SAMPLES),
            ],
            rate,
        }
    }

    /// Reads the deck's parameters from the bus and sets targets.
    pub fn read_bus(&mut self, bus: &ControlBus, deck: DeckId) {
        let gain = bus.get(deck, ParamAddress::Gain);
        self.gain.set_target(gain);
        let low = eq_db(bus.get(deck, ParamAddress::EqLow));
        let mid = eq_db(bus.get(deck, ParamAddress::EqMid));
        let high = eq_db(bus.get(deck, ParamAddress::EqHigh));
        self.eq_low = [Biquad::low_shelf(200.0, low, self.rate); 2];
        self.eq_mid = [Biquad::peaking(1_000.0, 1.0, mid, self.rate); 2];
        self.eq_high = [Biquad::high_shelf(4_000.0, high, self.rate); 2];
        self.cutoffs[0].set_target(bus.get(deck, ParamAddress::HpfCutoff));
        self.cutoffs[1].set_target(bus.get(deck, ParamAddress::LpfCutoff));
    }

    /// Processes a block: smoothing, EQ, cascaded filters, gain.
    ///
    /// Smoothers advance per sample; HPF/LPF coefficients are
    /// updated (state-preserving) when their smoothed cutoff moves
    /// beyond a deadband, snapped at the endpoints so the asymptotic
    /// approach stops triggering updates.
    pub fn process_block(&mut self, buf: &mut [f32]) {
        for chunk in buf.chunks_mut(BLOCK * 2) {
            for frame in chunk.chunks_mut(2) {
                let g = self.gain.tick();
                let h = self.cutoffs[0].tick();
                let l = self.cutoffs[1].tick();
                self.update_filters(h, l);

                for (ch, x) in frame.iter_mut().enumerate() {
                    let s = self.eq_low[ch].tick(*x);
                    let s = self.eq_mid[ch].tick(s);
                    let s = self.eq_high[ch].tick(s);
                    let s = self.hpf[ch][0].tick(s);
                    let s = self.hpf[ch][1].tick(s);
                    let s = self.lpf[ch][0].tick(s);
                    let s = self.lpf[ch][1].tick(s);
                    *x = s * g;
                }
            }
        }
    }

    /// Swaps HPF/LPF coefficients when the smoothed normalized
    /// cutoff moves beyond a deadband (biquad state is preserved).
    fn update_filters(&mut self, hpf_target: f32, lpf_target: f32) {
        let h = snap(hpf_target);
        let l = snap(lpf_target);
        if (h - self.hpf_norm).abs() > 0.002 {
            self.hpf_norm = h;
            let hz_h = hpf_hz(h);
            let f = Biquad::high_pass(hz_h, self.rate);
            self.hpf[0][0].retune(&f);
            self.hpf[0][1].retune(&f);
            self.hpf[1][0].retune(&f);
            self.hpf[1][1].retune(&f);
        }
        if (l - self.lpf_norm).abs() > 0.002 {
            self.lpf_norm = l;
            let hz_l = lpf_hz(l);
            let f = Biquad::low_pass(hz_l, self.rate);
            self.lpf[0][0].retune(&f);
            self.lpf[0][1].retune(&f);
            self.lpf[1][0].retune(&f);
            self.lpf[1][1].retune(&f);
        }
    }
}

/// Snaps near-0/near-1 smoother outputs to exact endpoints.
fn snap(v: f32) -> f32 {
    if v < 0.001 {
        0.0
    } else if v > 0.999 {
        1.0
    } else {
        v
    }
}

impl Biquad {
    /// Butterworth high-pass at `hz` (Q = 0.707).
    #[must_use]
    pub fn high_pass(hz: f32, rate: f32) -> Self {
        butter(hz, rate, true)
    }

    /// Butterworth low-pass at `hz` (Q = 0.707).
    #[must_use]
    pub fn low_pass(hz: f32, rate: f32) -> Self {
        butter(hz, rate, false)
    }
}

/// Butterworth biquad at `hz`.
fn butter(hz: f32, rate: f32, high: bool) -> Biquad {
    let w0 = 2.0 * std::f32::consts::PI * hz.clamp(10.0, rate / 2.0 - 1.0) / rate;
    let cw = w0.cos();
    let alpha = w0.sin() / 2.0_f32.sqrt();
    let (b0, b1, b2) = if high {
        (f32::midpoint(1.0, cw), -(1.0 + cw), f32::midpoint(1.0, cw))
    } else {
        (f32::midpoint(1.0, -cw), 1.0 - cw, f32::midpoint(1.0, -cw))
    };
    let a0 = 1.0 + alpha;
    Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: (-2.0 * cw) / a0,
        a2: (1.0 - alpha) / a0,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlBus;

    fn sine(freq: f32, rate: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "test index")]
                let t = i as f32 / rate;
                (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5
            })
            .collect()
    }

    fn rms(buf: &[f32]) -> f32 {
        #[expect(clippy::cast_precision_loss, reason = "test length is small")]
        let n = buf.len() as f32;
        (buf.iter().map(|x| x * x).sum::<f32>() / n).sqrt()
    }

    #[test]
    fn identity_chain_passes_signal() {
        // Given a unity deck chain.
        let mut chain = DeckChain::new(44_100.0, 1.0);

        // When processing a 440 Hz sine.
        let mut buf = sine(440.0, 44_100.0, 8_192);
        let expected = rms(&buf);
        chain.process_block(&mut buf);

        // Then the signal level is preserved.
        assert!((rms(&buf) - expected).abs() < expected * 0.05);
    }

    #[test]
    fn hpf_kills_low_frequency() {
        // Given a deck with HPF fully open (~2 kHz).
        let mut chain = DeckChain::new(44_100.0, 1.0);
        let mut bus = ControlBus::new();
        bus.set(DeckId::A, ParamAddress::HpfCutoff, 1.0);
        chain.read_bus(&bus, DeckId::A);

        // When processing a 100 Hz sine.
        let mut buf = sine(100.0, 44_100.0, 8_192);
        let before = rms(&buf);
        chain.process_block(&mut buf);

        // Then the settled tail is heavily attenuated.
        let tail = &buf[buf.len() - 4_096..];
        assert!(rms(tail) < before * 0.1, "{} vs {}", rms(tail), before);
    }

    #[test]
    fn lpf_kills_high_frequency() {
        // Given a deck with LPF fully closed (~1 kHz).
        let mut chain = DeckChain::new(44_100.0, 1.0);
        let mut bus = ControlBus::new();
        bus.set(DeckId::A, ParamAddress::LpfCutoff, 0.0);
        chain.read_bus(&bus, DeckId::A);

        // When processing a 10 kHz sine.
        let mut buf = sine(10_000.0, 44_100.0, 8_192);
        let before = rms(&buf);
        chain.process_block(&mut buf);

        // Then the settled tail is heavily attenuated.
        let tail = &buf[buf.len() - 4_096..];
        assert!(rms(tail) < before * 0.1, "{} vs {}", rms(tail), before);
    }

    #[test]
    fn gain_scales_output_after_smoothing() {
        // Given a deck with gain target 0.5.
        let mut chain = DeckChain::new(44_100.0, 1.0);
        let mut bus = ControlBus::new();
        bus.set(DeckId::A, ParamAddress::Gain, 0.5);
        chain.read_bus(&bus, DeckId::A);

        // When processing enough audio for the smoother to settle
        // (~10 tau ≈ 0.1 s at 44.1 kHz).
        let mut buf = sine(440.0, 44_100.0, 44_100);
        chain.process_block(&mut buf);

        // Then the tail is at ~half level (unity chain, pure gain).
        let tail = &buf[buf.len() - 4_096..];
        let reference = rms(&sine(440.0, 44_100.0, 4_096));
        assert!(
            (rms(tail) - reference * 0.5).abs() < reference * 0.05,
            "{} vs {}",
            rms(tail),
            reference * 0.5
        );
    }

    #[test]
    fn smoothing_bounds_step_discontinuity() {
        // Given a smoother with the deck's tau.
        let mut sm = Smoother::new(0.0, SMOOTH_SAMPLES);

        // When stepping the target to 1 and ticking one block.
        sm.set_target(1.0);
        let mut max_step = 0.0_f32;
        let mut prev = 0.0_f32;
        for _ in 0..BLOCK {
            let v = sm.tick();
            max_step = max_step.max((v - prev).abs());
            prev = v;
        }

        // Then no per-sample jump exceeds the one-pole increment.
        assert!(max_step < 0.02, "max step {max_step}");
    }
}
