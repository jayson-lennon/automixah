//! Transition presets: data-defined automation timelines.
//!
//! A preset is a [`PresetSpec`] — declarative parameters, serialized
//! as RON in the app bundle — compiled by [`compile_preset`] into a
//! concrete `Vec<ControlEvent>` timeline sampled at 1/4-beat
//! resolution. The compiled timeline feeds a
//! [`TimelineSource`](crate::automation::TimelineSource).
//!
//! Built-in specs (data, not code): [`PresetSpec::crossfade`],
//! [`PresetSpec::low_cut_blend`], [`PresetSpec::bass_swap`],
//! [`PresetSpec::cut`].

use crate::control::{ControlEvent, DeckId, ParamAddress};
use crate::timeline::types::{SessionTime, TransitionWindow};
use serde::{Deserialize, Serialize};

/// The declarative shape of one automation curve: which parameter to
/// sweep, over which normalized span, following which shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurveSpec {
    /// Deck whose parameter is swept.
    pub deck: DeckSerde,
    /// Parameter address.
    pub address: AddressSerde,
    /// Value at the window start.
    pub from: f32,
    /// Value at the window end.
    pub to: f32,
    /// Curve shape.
    pub shape: Shape,
}

/// Curve shape applied between `from` and `to`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Shape {
    /// Equal-power (cos/sin pair; constant perceived loudness).
    EqualPower,
    /// Linear interpolation.
    Linear,
}

/// Serializable mirrors of the control enums (RON-friendly).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DeckSerde {
    /// Deck A.
    A,
    /// Deck B.
    B,
}

/// Serializable parameter address.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AddressSerde {
    // variant names are the RON surface; PascalCase (Gain, EqLow)
    /// Gain.
    Gain,
    /// Low EQ.
    EqLow,
    /// Mid EQ.
    EqMid,
    /// High EQ.
    EqHigh,
    /// HPF cutoff.
    HpfCutoff,
    /// LPF cutoff.
    LpfCutoff,
}

impl DeckSerde {
    /// Converts to the engine enum.
    #[must_use]
    pub fn to_deck(self) -> DeckId {
        match self {
            Self::A => DeckId::A,
            Self::B => DeckId::B,
        }
    }
}

impl AddressSerde {
    /// Converts to the engine enum.
    #[must_use]
    pub fn to_address(self) -> ParamAddress {
        match self {
            Self::Gain => ParamAddress::Gain,
            Self::EqLow => ParamAddress::EqLow,
            Self::EqMid => ParamAddress::EqMid,
            Self::EqHigh => ParamAddress::EqHigh,
            Self::HpfCutoff => ParamAddress::HpfCutoff,
            Self::LpfCutoff => ParamAddress::LpfCutoff,
        }
    }
}

/// A complete preset: its curves and window length in beats.
///
/// Serialized as RON; the four built-ins are available as `const`
/// constructors and exposed via [`preset_specs`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetSpec {
    /// Preset name (matches the planner's [`PresetName`]).
    pub name: String,
    /// Window length in beats (must match the planner's placement).
    pub beats: usize,
    /// The curves driven across the window.
    pub curves: Vec<CurveSpec>,
}

impl PresetSpec {
    /// Classic equal-power gain crossfade over 32 beats (8 bars).
    #[must_use]
    pub fn crossfade() -> Self {
        Self {
            name: "Crossfade".into(),
            beats: 32,
            curves: vec![
                CurveSpec {
                    deck: DeckSerde::A,
                    address: AddressSerde::Gain,
                    from: 1.0,
                    to: 0.0,
                    shape: Shape::EqualPower,
                },
                CurveSpec {
                    deck: DeckSerde::B,
                    address: AddressSerde::Gain,
                    from: 0.0,
                    to: 1.0,
                    shape: Shape::EqualPower,
                },
            ],
        }
    }

    /// B fades in under a high-pass sweep that opens; A fades out.
    #[must_use]
    pub fn low_cut_blend() -> Self {
        Self {
            name: "LowCutBlend".into(),
            beats: 32,
            curves: vec![
                CurveSpec {
                    deck: DeckSerde::A,
                    address: AddressSerde::Gain,
                    from: 1.0,
                    to: 0.0,
                    shape: Shape::EqualPower,
                },
                CurveSpec {
                    deck: DeckSerde::B,
                    address: AddressSerde::Gain,
                    from: 0.0,
                    to: 1.0,
                    shape: Shape::EqualPower,
                },
                CurveSpec {
                    deck: DeckSerde::B,
                    address: AddressSerde::HpfCutoff,
                    // 0.0 = 20 Hz (bypass); sweep opens to ~0.72
                    // (≈300 Hz) then closes back to bypass by the end.
                    from: 0.72,
                    to: 0.0,
                    shape: Shape::Linear,
                },
            ],
        }
    }

    /// Low-band swap: A's low EQ dips to kill, B's rises from kill;
    /// gains crossfade over the same window.
    #[must_use]
    pub fn bass_swap() -> Self {
        Self {
            name: "BassSwap".into(),
            beats: 32,
            curves: vec![
                CurveSpec {
                    deck: DeckSerde::A,
                    address: AddressSerde::Gain,
                    from: 1.0,
                    to: 0.0,
                    shape: Shape::EqualPower,
                },
                CurveSpec {
                    deck: DeckSerde::B,
                    address: AddressSerde::Gain,
                    from: 0.0,
                    to: 1.0,
                    shape: Shape::EqualPower,
                },
                CurveSpec {
                    deck: DeckSerde::A,
                    address: AddressSerde::EqLow,
                    from: 0.5,
                    to: 0.0,
                    shape: Shape::Linear,
                },
                CurveSpec {
                    deck: DeckSerde::B,
                    address: AddressSerde::EqLow,
                    from: 0.0,
                    to: 0.5,
                    shape: Shape::Linear,
                },
            ],
        }
    }

    /// Hard cut on the downbeat with a 1-beat gain fade (both decks
    /// move over one beat at the end of the window).
    #[must_use]
    pub fn cut() -> Self {
        Self {
            name: "Cut".into(),
            beats: 1,
            curves: vec![
                CurveSpec {
                    deck: DeckSerde::A,
                    address: AddressSerde::Gain,
                    from: 1.0,
                    to: 0.0,
                    shape: Shape::Linear,
                },
                CurveSpec {
                    deck: DeckSerde::B,
                    address: AddressSerde::Gain,
                    from: 0.0,
                    to: 1.0,
                    shape: Shape::Linear,
                },
            ],
        }
    }
}

/// The four built-in preset specs.
#[must_use]
pub fn preset_specs() -> Vec<PresetSpec> {
    vec![
        PresetSpec::crossfade(),
        PresetSpec::low_cut_blend(),
        PresetSpec::bass_swap(),
        PresetSpec::cut(),
    ]
}

/// Samples a curve shape at progress `t`.
pub(crate) fn shape_value(shape: Shape, from: f32, to: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match shape {
        Shape::Linear => from + (to - from) * t,
        Shape::EqualPower => {
            // Complementary fades (1→0 and 0→1) become a cos/sin
            // pair: constant summed power across the transition.
            let theta = t * std::f32::consts::FRAC_PI_2;
            from * theta.cos() + to * theta.sin()
        }
    }
}

/// Compiles a preset spec into concrete control events for a placed
/// window.
///
/// Events are sampled at 1/4-beat resolution across the window: the
/// session-BPM beat length in samples is quartered, and each curve
/// emits `(beats × 4 + 1)` events (endpoints included).
#[must_use]
pub fn compile_preset(
    spec: &PresetSpec,
    window: TransitionWindow,
    session_bpm: f32,
    sample_rate: u32,
) -> Vec<ControlEvent> {
    #[expect(clippy::cast_precision_loss, reason = "sample rates are exact in f32")]
    let beat_samples = 60.0 / session_bpm * sample_rate as f32;
    let quarter = (beat_samples / 4.0).round().max(1.0);
    let steps = spec.beats * 4;

    let mut events = Vec::with_capacity(spec.curves.len() * (steps + 1));
    for curve in &spec.curves {
        for i in 0..=steps {
            #[expect(clippy::cast_precision_loss, reason = "step indices are small")]
            let t = i as f32 / steps as f32;
            let value = shape_value(curve.shape, curve.from, curve.to, t);
            #[expect(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "step indices and quarter-beat offsets are small"
            )]
            let offset = (i as f32 * quarter).round() as u64;
            events.push(ControlEvent {
                deck: curve.deck.to_deck(),
                address: curve.address.to_address(),
                value: value.clamp(0.0, 1.0),
                time: SessionTime(window.start.0 + offset.min(window.len_samples())),
            });
        }
    }
    events
}

/// The equal-power pair check: gains sum to ≈ constant power.
#[cfg(test)]
pub(crate) fn equal_power_sum(a: f32, b: f32) -> f32 {
    a.mul_add(a, b * b).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(len: u64) -> TransitionWindow {
        TransitionWindow {
            start: SessionTime(1000),
            end: SessionTime(1000 + len),
        }
    }

    #[test]
    fn crossfade_compiles_to_quarter_beat_events() {
        // Given a crossfade preset at 120 BPM (0.5 s beats).
        let spec = PresetSpec::crossfade();

        // When compiling over an 8-bar window.
        let events = compile_preset(&spec, window(705_600), 120.0, 44_100);

        // Then each curve has 32 beats × 4 + 1 = 129 events.
        let gain_a = events
            .iter()
            .filter(|e| e.deck == DeckId::A && e.address == ParamAddress::Gain)
            .count();
        assert_eq!(gain_a, 129);
    }

    #[test]
    fn crossfade_gain_pair_is_equal_power() {
        // Given a compiled crossfade.
        let spec = PresetSpec::crossfade();
        let events = compile_preset(&spec, window(705_600), 120.0, 44_100);

        // When sampling the gain pair at each step.
        let a: Vec<f32> = events
            .iter()
            .filter(|e| e.deck == DeckId::A && e.address == ParamAddress::Gain)
            .map(|e| e.value)
            .collect();
        let b: Vec<f32> = events
            .iter()
            .filter(|e| e.deck == DeckId::B && e.address == ParamAddress::Gain)
            .map(|e| e.value)
            .collect();

        // Then a² + b² ≈ 1 (constant power) throughout.
        for (a, b) in a.iter().zip(&b) {
            assert!(
                (equal_power_sum(*a, *b) - 1.0).abs() < 0.02,
                "pair ({a}, {b}) not equal-power"
            );
        }
    }

    #[test]
    fn cut_fades_over_one_beat() {
        // Given a Cut preset.
        let spec = PresetSpec::cut();

        // When compiling at 120 BPM.
        let events = compile_preset(&spec, window(22_050), 120.0, 44_100);

        // Then each curve has 1 beat × 4 + 1 = 5 events.
        let gain_a = events
            .iter()
            .filter(|e| e.deck == DeckId::A && e.address == ParamAddress::Gain)
            .count();
        assert_eq!(gain_a, 5);
    }

    #[test]
    fn bass_swap_sweeps_low_eq_in_opposition() {
        // Given a compiled BassSwap.
        let spec = PresetSpec::bass_swap();
        let events = compile_preset(&spec, window(705_600), 120.0, 44_100);

        // When reading the EqLow pair at the midpoint.
        let midpoint = 705_600 / 2;
        let nearest = |deck: DeckId| {
            events
                .iter()
                .filter(|e| e.deck == deck && e.address == ParamAddress::EqLow)
                .min_by_key(|e| e.time.0.abs_diff(1000 + midpoint))
                .map_or(0.5, |e| e.value)
        };
        let a = nearest(DeckId::A);
        let b = nearest(DeckId::B);

        // Then A's low is dying while B's is rising (sum ≈ 0.5).
        assert!((a + b - 0.5).abs() < 0.05, "a={a} b={b}");
    }

    #[test]
    fn low_cut_blend_sweeps_hpf_then_opens() {
        // Given a compiled LowCutBlend.
        let spec = PresetSpec::low_cut_blend();
        let events = compile_preset(&spec, window(705_600), 120.0, 44_100);

        // When reading B's HPF at start and end.
        let hpf = |t: u64| {
            events
                .iter()
                .filter(|e| e.address == ParamAddress::HpfCutoff)
                .min_by_key(|e| e.time.0.abs_diff(t))
                .map_or(0.0, |e| e.value)
        };

        // Then the sweep starts open and ends bypassed.
        assert!((hpf(1000) - 0.72).abs() < 0.01);
        assert!(hpf(1000 + 705_600) < 0.01);
    }

    #[test]
    fn presets_roundtrip_through_ron() {
        // Given the built-in specs.
        for spec in preset_specs() {
            // When serializing to RON and back.
            let ron = ron::to_string(&spec).expect("ron ser");
            let back: PresetSpec = ron::from_str(&ron).expect("ron de");

            // Then the spec survives.
            assert_eq!(back, spec);
        }
    }
}
