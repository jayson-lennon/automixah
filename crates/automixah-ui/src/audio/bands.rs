//! Bessel-4 band filters — a port of Mixxx's waveform band splitting.
//!
//! Mixxx splits the signal into low (<600 Hz), mid (600–4000 Hz), and high
//! (>4000 Hz) bands using order-4 Bessel IIR filters (`EngineFilterBessel4{Low,
//! Band,High}`), designed by fidlib with a bilinear transform. This module
//! reproduces that design and the exact per-sample cascade Mixxx runs.
//!
//! Design path (fidlib `fidmkf.h`):
//! 1. Normalized Bessel-4 poles (two conjugate pairs).
//! 2. `prewarp(f) = tan(π·f/rate)/π`, then poles scaled by `2·tan(π·f/rate)`
//!    (low/high) or band-expanded around `w0 = 2π·√(f1·f2)` with
//!    `bw = π·(f2−f1)` (band).
//! 3. Bilinear transform `z = (2+s)/(2−s)`.
//! 4. Each conjugate pole pair becomes one biquad section; zeros land at
//!    z = −1 (low: `(1+z⁻¹)²`) or z = +1 (high: `(1−z⁻¹)²`), and the band
//!    filter mixes both (first half +1, second half −1).
//! 5. Gain normalizes the response to 1.0 at DC (low), Nyquist (high), or
//!    the in-band peak (band, golden-section search like fidlib's
//!    `search_peak`).
//!
//! The per-sample recursion (Mixxx `EngineFilterIIR<4/8, IIR_LP/BP/HP>::
//! processSample`) reduces to, per section:
//!
//! ```text
//! y_new  = x − a1·y1 − a2·y2          // a1 = −2·Re(pole), a2 = |pole|²
//! out    = y2 + z2·y1 + y_new         // z2 = ±2 (FIR middle coefficient)
//! ```
//!
//! Zero-initialized state is exactly Mixxx's `assumeSettled()` behavior
//! (settled for silence); the filter ramps only when real audio starts,
//! which Mixxx accepts for waveform rendering.

/// Mixxx's band corners (`analyzerwaveform.cpp`).
pub const LOW_MID_HZ: f64 = 600.0;
pub const MID_HIGH_HZ: f64 = 4000.0;

/// Normalized Bessel order-4 poles (two conjugate pairs), fidlib `bessel_4`.
const BESSEL4_POLES: [(f64, f64); 2] = [
    (-0.995_208_764_35, 1.257_105_739_45),
    (-1.370_067_830_55, 0.410_249_717_494),
];

// ── minimal complex arithmetic ───────────────────────────────���───────────

type Cx = (f64, f64);

fn cadd(a: Cx, b: Cx) -> Cx {
    (a.0 + b.0, a.1 + b.1)
}

fn cmul(a: Cx, b: Cx) -> Cx {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn cmulr(a: Cx, r: f64) -> Cx {
    (a.0 * r, a.1 * r)
}

fn cdiv(a: Cx, b: Cx) -> Cx {
    let d = b.0 * b.0 + b.1 * b.1;
    ((a.0 * b.0 + a.1 * b.1) / d, (a.1 * b.0 - a.0 * b.1) / d)
}

#[expect(
    dead_code,
    reason = "csqrt sibling kept for fidlib parity; used in exact_pole consumers later"
)]
fn cconj(a: Cx) -> Cx {
    (a.0, -a.1)
}

fn cabs2(a: Cx) -> f64 {
    a.0 * a.0 + a.1 * a.1
}

fn csqrt(a: Cx) -> Cx {
    // fidlib c_sqrt: my_sqrt clamps negatives to 0; sign from the imaginary part.
    let my_sqrt = |v: f64| if v <= 0.0 { 0.0 } else { v.sqrt() };
    let m = a.0.hypot(a.1);
    let r = my_sqrt((m + a.0) * 0.5);
    let mut i = my_sqrt((m - a.0) * 0.5);
    if a.1 < 0.0 {
        i = -i;
    }
    (r, i)
}

/// Bilinear transform `(2+s)/(2−s)`.
fn bilinear(s: Cx) -> Cx {
    cdiv(cadd((2.0, 0.0), s), cadd((2.0, 0.0), (-s.0, -s.1)))
}

/// `prewarp(f) = tan(π·f/rate)/π` — frequency as a proportion of the rate.
fn prewarp(freq_hz: f64, rate: f64) -> f64 {
    let x = std::f64::consts::PI * freq_hz / rate;
    x.tan() / std::f64::consts::PI
}

// ── cascade ──────────────────────────────────────────────────────────────

/// One biquad section of the cascade.
#[derive(Debug, Clone, Copy)]
struct Section {
    /// Newer-history IIR coefficient (`−2·Re(pole)`).
    a1: f64,
    /// Older-history IIR coefficient (`|pole|²`).
    a2: f64,
    /// FIR middle coefficient: `+2` zeros at z=−1, `−2` zeros at z=+1.
    z2: f64,
    y1: f64,
    y2: f64,
}

impl Section {
    fn from_pole(pole: Cx, z2: f64) -> Self {
        Self {
            a1: -2.0 * pole.0,
            a2: cabs2(pole),
            z2,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = x - self.a1 * self.y1 - self.a2 * self.y2;
        let out = self.y2 + self.z2 * self.y1 + y;
        self.y2 = self.y1;
        self.y1 = y;
        out
    }

    /// Recovers the exact complex pole this section was built from.
    fn exact_pole(&self) -> Cx {
        let re = -self.a1 / 2.0;
        let im = (self.a2 - re * re).max(0.0).sqrt();
        (re, im)
    }
}

/// A designed filter: gain + biquad cascade, stateful per channel.
#[derive(Debug, Clone)]
pub struct Cascade {
    gain: f64,
    sections: Vec<Section>,
}

impl Cascade {
    fn new(gain: f64, sections: Vec<Section>) -> Self {
        Self { gain, sections }
    }

    /// Processes one sample (state advances).
    pub fn process(&mut self, x: f64) -> f64 {
        let mut v = x * self.gain;
        for section in &mut self.sections {
            v = section.run(v);
        }
        v
    }

    /// Analytic magnitude response `|H(θ)|` (state-independent), θ in radians.
    ///
    /// Matches fidlib's `fid_response`: evaluates the cascade at
    /// `e^{+jθ}` with exact complex section poles (the section pole from
    /// `from_pole` paired with its conjugate).
    #[must_use]
    pub fn response(&self, theta: f64) -> f64 {
        let e = (theta.cos(), theta.sin());
        let mut mag2 = self.gain * self.gain;
        for s in &self.sections {
            // FIR: (1 ∓ e^{jθ})² — z2=+2 ⇒ (1+e)², z2=−2 ⇒ (1−e)².
            let base = if s.z2 > 0.0 {
                cadd((1.0, 0.0), e)
            } else {
                cadd((1.0, 0.0), MapT::negate(e))
            };
            let n = cabs2(cmul(base, base));
            // Denominator: exact pole from the section coefficients, evaluated
            // against the pole and its conjugate: |1 − p·e| · |1 − p̄·e|.
            let p = s.exact_pole();
            let pc = (p.0, -p.1);
            let d = cabs2(cmul(
                cadd((1.0, 0.0), MapT::negate(cmul(p, e))),
                cadd((1.0, 0.0), MapT::negate(cmul(pc, e))),
            ));
            mag2 *= n / d;
        }
        mag2.sqrt()
    }

    /// Designs the lowpass Bessel-4 at `corner_hz`.
    #[must_use]
    pub fn lowpass(rate: f64, corner_hz: f64) -> Self {
        let w = 2.0 * (std::f64::consts::PI * corner_hz / rate).tan();
        let poles: Vec<Cx> = BESSEL4_POLES
            .iter()
            .map(|&(re, im)| bilinear(cmulr((re, im), w)))
            .collect();
        let sections = poles.iter().map(|&p| Section::from_pole(p, 2.0)).collect();
        let dc = poles
            .iter()
            .map(|&p| cabs2(cadd((1.0, 0.0), (-p.0, -p.1))))
            .product::<f64>()
            / 16.0; // ∏|1−p|² / (numerator 2² per section)
        Self::new(dc, sections)
    }

    /// Designs the highpass Bessel-4 at `corner_hz`.
    #[must_use]
    pub fn highpass(rate: f64, corner_hz: f64) -> Self {
        let w = 2.0 * (std::f64::consts::PI * corner_hz / rate).tan();
        let poles: Vec<Cx> = BESSEL4_POLES
            .iter()
            .map(|&(re, im)| {
                let p = (re, im);
                bilinear(cmulr(cdiv((1.0, 0.0), p), w)) // (1/p)·w
            })
            .collect();
        let sections = poles.iter().map(|&p| Section::from_pole(p, -2.0)).collect();
        let nyq = poles
            .iter()
            .map(|&p| cabs2(cadd((1.0, 0.0), p)))
            .product::<f64>()
            / 16.0;
        Self::new(nyq, sections)
    }

    /// Designs the bandpass Bessel-4 between `f1_hz` and `f2_hz`.
    #[must_use]
    pub fn bandpass(rate: f64, f1_hz: f64, f2_hz: f64) -> Self {
        let pw1 = prewarp(f1_hz, rate);
        let pw2 = prewarp(f2_hz, rate);
        let w0 = 2.0 * std::f64::consts::PI * (pw1 * pw2).sqrt();
        let bw = std::f64::consts::PI * (pw2 - pw1);

        let mut poles = Vec::with_capacity(4);
        for &(re, im) in &BESSEL4_POLES {
            // Per Bessel pole: hba = p·bw; pole± = hba·(1 ± t),
            // t = sqrt(1 − (w0/hba)²) with fidlib's complex csqrt.
            let hba = cmulr((re, im), bw);
            let ratio = cdiv((w0, 0.0), hba);
            let t = csqrt(cadd((1.0, 0.0), MapT::negate(cmul(ratio, ratio))));
            let pole_a = cadd(hba, cmul(hba, t));
            let pole_b = cadd(hba, MapT::negate(cmul(hba, t)));
            poles.push(pole_a);
            poles.push(pole_b);
        }
        //
        // Zero layout (fidlib `bandpass` + `s2z_bilinear` + `z2fidfilter`):
        // the first half of the zeros map to z = +1 (FIR (1 − z⁻¹)², z2 = −2)
        // and the second half to z = −1 (FIR (1 + z⁻¹)², z2 = +2), paired
        // with the expanded poles in slot order [pa0, pb0, pa1, pb1].
        let z2_layout = [-2.0, -2.0, 2.0, 2.0];
        let sections = poles
            .iter()
            .zip(z2_layout)
            .map(|(&p, z2)| Section::from_pole(bilinear(p), z2))
            .collect();

        let mut design = Self::new(1.0, sections);
        let peak = search_peak(&design, f1_hz / rate, f2_hz / rate);
        design.gain = 1.0 / peak;
        design
    }
}

/// Golden-section search for the peak magnitude response in `[lo, hi]`
/// (cycles/sample), mirroring fidlib's `search_peak` normalization.
fn search_peak(design: &Cascade, lo: f64, hi: f64) -> f64 {
    let two_pi = 2.0 * std::f64::consts::PI;
    let mut lo = lo;
    let mut hi = hi;
    let phi = 0.618_033_988_749_894_9;
    for _ in 0..80 {
        let a = hi - phi * (hi - lo);
        let b = lo + phi * (hi - lo);
        if design.response(two_pi * a) < design.response(two_pi * b) {
            lo = a;
        } else {
            hi = b;
        }
    }
    let mid = two_pi * (lo + hi) / 2.0;
    design.response(mid)
}

/// Maps a tuple (used above for complex arithmetic readability).
trait MapT {
    fn negate(self) -> Cx;
}

impl MapT for Cx {
    fn negate(self) -> Cx {
        (-self.0, -self.1)
    }
}

// ── band splitter ────────────────────────────────────────────────────────

/// The three-band splitter Mixxx uses for waveform rendering: one filter set
/// per stereo channel, processing interleaved frames.
#[derive(Debug, Clone)]
pub struct BandSplitter {
    low: [Cascade; 2],
    mid: [Cascade; 2],
    high: [Cascade; 2],
}

impl BandSplitter {
    /// Creates a splitter with Mixxx's 600 Hz / 4000 Hz corners.
    #[must_use]
    pub fn new(rate: f64) -> Self {
        Self {
            low: [
                Cascade::lowpass(rate, LOW_MID_HZ),
                Cascade::lowpass(rate, LOW_MID_HZ),
            ],
            mid: [
                Cascade::bandpass(rate, LOW_MID_HZ, MID_HIGH_HZ),
                Cascade::bandpass(rate, LOW_MID_HZ, MID_HIGH_HZ),
            ],
            high: [
                Cascade::highpass(rate, MID_HIGH_HZ),
                Cascade::highpass(rate, MID_HIGH_HZ),
            ],
        }
    }

    /// Processes one stereo frame, returning `(low, mid, high)` per channel.
    pub fn process_frame(&mut self, l: f32, r: f32) -> [[f32; 3]; 2] {
        [self.bands(0, f64::from(l)), self.bands(1, f64::from(r))]
    }

    fn bands(&mut self, ch: usize, x: f64) -> [f32; 3] {
        [
            self.low[ch].process(x) as f32,
            self.mid[ch].process(x) as f32,
            self.high[ch].process(x) as f32,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 44_100.0;

    /// Deterministic LCG white noise (no external rand dependency).
    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as f32 / (u32::MAX >> 1) as f32) - 1.0
        }
    }

    fn band_rms(freq: f64, band_index: usize) -> f64 {
        let mut splitter = BandSplitter::new(RATE);
        let period = RATE / freq;
        let n = 44_100 * 4; // 4 s
        let settle = 44_100; // skip the first second
        let mut acc = 0.0;
        let mut count = 0.0;
        for i in 0..n {
            let t = i as f64 / period;
            let sample = (2.0 * std::f64::consts::PI * t).sin() as f32;
            let bands = splitter.process_frame(sample, sample);
            if i >= settle {
                acc += f64::from(bands[0][band_index]) * f64::from(bands[0][band_index]);
                count += 1.0;
            }
        }
        (acc / count).sqrt()
    }

    // Given a 150 Hz sine.
    // When split into bands.
    // Then the low band dominates by >20×.
    #[test]
    fn low_band_captures_bass() {
        let low = band_rms(150.0, 0);
        let mid = band_rms(150.0, 1);
        let high = band_rms(150.0, 2);
        assert!(low > 20.0 * mid, "low {low} vs mid {mid}");
        assert!(low > 20.0 * high, "low {low} vs high {high}");
    }

    // Given a 1 kHz sine.
    // When split into bands.
    // Then the mid band dominates: BP@600-4000 peak-normalized response
    // is 0.95 at 1 kHz vs 0.34 for the LP skirt (Bessel-4 rolloff).
    #[test]
    fn mid_band_captures_midrange() {
        let low = band_rms(1_000.0, 0);
        let mid = band_rms(1_000.0, 1);
        let high = band_rms(1_000.0, 2);
        assert!(mid > 2.5 * low, "mid {mid} vs low {low}");
        assert!(mid > 20.0 * high, "mid {mid} vs high {high}");
    }

    // Given an 8 kHz sine.
    // When split into bands.
    // Then the high band dominates by >6x (HP@4000 at 8 kHz is 0.94
    // while BP's upper skirt is 0.11).
    #[test]
    fn high_band_captures_treble() {
        let low = band_rms(8_000.0, 0);
        let mid = band_rms(8_000.0, 1);
        let high = band_rms(8_000.0, 2);
        assert!(high > 6.0 * mid, "high {high} vs mid {mid}");
        assert!(high > 20.0 * low, "high {high} vs low {low}");
    }

    // Given a constant DC signal.
    // When lowpassed.
    // Then the settled output equals the input (unity DC gain).
    #[test]
    fn lowpass_settles_to_unity_dc() {
        let mut lp = Cascade::lowpass(RATE, LOW_MID_HZ);
        let mut out = 0.0;
        for _ in 0..44_100 {
            out = lp.process(0.5);
        }
        assert!((out - 0.5).abs() < 1e-6, "settled DC {out}");
    }

    // Given an alternating ±1 signal (Nyquist).
    // When highpassed.
    // Then the settled output alternates at full amplitude (unity gain).
    #[test]
    fn highpass_settles_to_unity_nyquist() {
        let mut hp = Cascade::highpass(RATE, MID_HIGH_HZ);
        let mut last = 0.0;
        for i in 0..44_100 {
            let x = if i % 2 == 0 { 1.0 } else { -1.0 };
            last = hp.process(x);
        }
        assert!((last.abs() - 1.0).abs() < 1e-3, "settled Nyquist {last}");
    }

    // Given a bandpass design.
    // When its peak in-band response is measured.
    // Then it is normalized to ≈1.0.
    #[test]
    fn bandpass_peak_is_normalized() {
        let bp = Cascade::bandpass(RATE, LOW_MID_HZ, MID_HIGH_HZ);
        let mut best = 0.0_f64;
        for i in 0..2000 {
            let f = LOW_MID_HZ + (MID_HIGH_HZ - LOW_MID_HZ) * i as f64 / 1999.0;
            best = best.max(bp.response(2.0 * std::f64::consts::PI * f / RATE));
        }
        assert!((best - 1.0).abs() < 0.01, "peak {best}");
    }

    // Given white noise through the splitter.
    // When any sample is checked.
    // Then outputs stay finite (no instability).
    #[test]
    fn splitter_stays_stable_on_noise() {
        let mut splitter = BandSplitter::new(RATE);
        let mut rng = Lcg(0x5EED);
        for _ in 0..44_100 {
            let s = rng.next_f32();
            let bands = splitter.process_frame(s, s);
            for ch in bands {
                for v in ch {
                    assert!(v.is_finite());
                }
            }
        }
    }
}
