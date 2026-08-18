//! Role-addressed automation pairs for transitions.
//!
//! A [`TransitionSpec`] is the authored half of a transition: one
//! outgoing-deck automation (the outro) and one incoming-deck
//! automation (the intro), always authored together. Curves address
//! decks by *role* — `Outgoing`/`Incoming` — not by literal deck id,
//! because physical decks alternate with segment parity (segment 0,
//! 3, 5 … on deck A; 1, 4, 6 … on deck B). Roles are mapped to
//! literal decks at compile time (see [`compile_transition`]).
//!
//! Serialized as RON; the built-in default is a 16-bar equal-power
//! crossfade. This is the data-driven surface future MIDI control
//! drives: a hardware knob targets a role + address, not a deck.

use serde::{Deserialize, Serialize};

use crate::control::{ControlEvent, DeckId, ParamAddress};
use crate::timeline::types::{SessionTime, TransitionWindow};

use super::presets::{AddressSerde, Shape, shape_value};

/// Which deck a curve targets, relative to the transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum RoleSerde {
    /// The outgoing deck (fading out / handing over).
    Outgoing,
    /// The incoming deck (fading in / taking over).
    Incoming,
}

/// One curve of a transition pair, addressed by role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionCurve {
    /// Deck role this curve drives.
    pub role: RoleSerde,
    /// Parameter address.
    pub address: AddressSerde,
    /// Value at the window start.
    pub from: f32,
    /// Value at the window end.
    pub to: f32,
    /// Curve shape.
    pub shape: Shape,
}

/// An authored intro/outro automation pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionSpec {
    /// Pair name (surfaces in plan logs).
    pub name: String,
    /// Window length in beats (16 bars = 64 beats at 4/4).
    pub beats: usize,
    /// The curves driven across the window.
    pub curves: Vec<TransitionCurve>,
}

/// The default pair: a 16-bar (64-beat) equal-power crossfade.
#[must_use]
pub fn long_crossfade() -> TransitionSpec {
    TransitionSpec {
        name: "LongCrossfade".into(),
        beats: 64,
        curves: vec![
            TransitionCurve {
                role: RoleSerde::Outgoing,
                address: AddressSerde::Gain,
                from: 1.0,
                to: 0.0,
                shape: Shape::EqualPower,
            },
            TransitionCurve {
                role: RoleSerde::Incoming,
                address: AddressSerde::Gain,
                from: 0.0,
                to: 1.0,
                shape: Shape::EqualPower,
            },
        ],
    }
}

/// The default pair (16-bar equal-power crossfade).
#[must_use]
pub fn default_pair() -> TransitionSpec {
    long_crossfade()
}

/// Validation errors for an authored pair.
#[derive(Debug, PartialEq, Eq)]
pub enum TransitionSpecError {
    /// `name` empty.
    EmptyName,
    /// `beats` outside `[1, 256]`.
    BadBeats(usize),
    /// A curve value was NaN/infinite.
    NonFiniteCurve(usize),
    /// No curves at all.
    NoCurves,
}

impl std::fmt::Display for TransitionSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "name must be non-empty"),
            Self::BadBeats(b) => write!(f, "beats {b} outside [1, 256]"),
            Self::NonFiniteCurve(i) => write!(f, "curve {i} has non-finite values"),
            Self::NoCurves => write!(f, "spec has no curves"),
        }
    }
}

impl TransitionSpec {
    /// The built-in default pair (16-bar equal-power crossfade).
    #[must_use]
    pub fn default_pair() -> Self {
        long_crossfade()
    }

    /// Validates the pair's shape.
    ///
    /// # Errors
    ///
    /// Returns the first violation found.
    pub fn validate(&self) -> Result<(), TransitionSpecError> {
        if self.name.trim().is_empty() {
            return Err(TransitionSpecError::EmptyName);
        }
        if self.beats == 0 || self.beats > 256 {
            return Err(TransitionSpecError::BadBeats(self.beats));
        }
        if self.curves.is_empty() {
            return Err(TransitionSpecError::NoCurves);
        }
        for (i, c) in self.curves.iter().enumerate() {
            if !c.from.is_finite() || !c.to.is_finite() {
                return Err(TransitionSpecError::NonFiniteCurve(i));
            }
        }
        Ok(())
    }

    /// Parses a RON pair, validating after decode.
    ///
    /// # Errors
    ///
    /// Returns the RON error or the first validation violation.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        let spec: Self = ron::from_str(text).map_err(|e| format!("RON parse: {e}"))?;
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
    }
}

/// Compiles a pair into control events for one transition.
///
/// `outgoing_deck` is the physical deck of the outgoing segment;
/// `Outgoing` curves address it and `Incoming` curves address the
/// other deck. Events step at quarter-beat resolution across the
/// window, exactly like [`super::presets::compile_preset`].
#[must_use]
pub fn compile_transition(
    spec: &TransitionSpec,
    window: TransitionWindow,
    session_bpm: f32,
    sample_rate: u32,
    outgoing_deck: DeckId,
) -> Vec<ControlEvent> {
    #[expect(clippy::cast_precision_loss, reason = "sample rates are exact in f32")]
    let beat_samples = 60.0 / session_bpm * sample_rate as f32;
    let quarter = (beat_samples / 4.0).round().max(1.0);
    let steps = spec.beats * 4;

    let incoming_deck = match outgoing_deck {
        DeckId::A => DeckId::B,
        DeckId::B => DeckId::A,
    };

    let mut events = Vec::with_capacity(spec.curves.len() * (steps + 1));
    for curve in &spec.curves {
        let deck = match curve.role {
            RoleSerde::Outgoing => outgoing_deck,
            RoleSerde::Incoming => incoming_deck,
        };
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
                deck,
                address: curve.address.to_address(),
                value: value.clamp(0.0, 1.0),
                time: SessionTime(window.start.0 + offset.min(window.len_samples())),
            });
        }
    }
    events
}

/// The address of a curve (test/inspection helper).
#[must_use]
pub fn curve_address(curve: &TransitionCurve) -> ParamAddress {
    curve.address.to_address()
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
    fn default_pair_is_16_bars_equal_power() {
        // Given the built-in default.
        let spec = long_crossfade();

        // Then it spans 64 beats with complementary gain curves.
        assert_eq!(spec.beats, 64);
        assert_eq!(spec.curves.len(), 2);
        let out = &spec.curves[0];
        let inc = &spec.curves[1];
        assert_eq!(out.role, RoleSerde::Outgoing);
        assert_eq!(out.from, 1.0);
        assert_eq!(out.to, 0.0);
        assert_eq!(inc.role, RoleSerde::Incoming);
        assert_eq!(inc.from, 0.0);
        assert_eq!(inc.to, 1.0);
    }

    #[test]
    fn ron_round_trip_preserves_pair() {
        // Given the default pair serialized to RON.
        let text = ron::to_string(&long_crossfade()).expect("ser");

        // When parsing back.
        let back = TransitionSpec::from_ron(&text).expect("de");

        // Then it equals the original.
        assert_eq!(back, long_crossfade());
    }

    #[test]
    fn invalid_specs_are_rejected_with_reason() {
        // Given a spec with zero beats.
        let mut spec = long_crossfade();
        spec.beats = 0;
        // Then it is rejected.
        assert_eq!(
            TransitionSpec::from_ron(&ron::to_string(&spec).unwrap()),
            Err("beats 0 outside [1, 256]".into())
        );

        // Given a spec with an empty name.
        let mut spec = long_crossfade();
        spec.name = "  ".into();
        assert_eq!(
            TransitionSpec::from_ron(&ron::to_string(&spec).unwrap()),
            Err("name must be non-empty".into())
        );

        // Given a spec with no curves.
        let mut spec = long_crossfade();
        spec.curves.clear();
        assert_eq!(
            TransitionSpec::from_ron(&ron::to_string(&spec).unwrap()),
            Err("spec has no curves".into())
        );
    }

    #[test]
    fn roles_map_to_decks_by_parity() {
        // Given the default pair compiled with outgoing = deck A.
        let events_a = compile_transition(
            &long_crossfade(),
            window(44_100 * 30),
            120.0,
            44_100,
            DeckId::A,
        );
        // And with outgoing = deck B.
        let events_b = compile_transition(
            &long_crossfade(),
            window(44_100 * 30),
            120.0,
            44_100,
            DeckId::B,
        );

        // Then in the first case A fades out and B fades in.
        let a_gains: Vec<f32> = events_a
            .iter()
            .filter(|e| e.deck == DeckId::A)
            .map(|e| e.value)
            .collect();
        let b_gains: Vec<f32> = events_a
            .iter()
            .filter(|e| e.deck == DeckId::B)
            .map(|e| e.value)
            .collect();
        assert_eq!(a_gains.first(), Some(&1.0));
        assert_eq!(a_gains.last(), Some(&0.0));
        assert_eq!(b_gains.first(), Some(&0.0));
        assert_eq!(b_gains.last(), Some(&1.0));

        // And in the second case the mapping inverts.
        let a_gains: Vec<f32> = events_b
            .iter()
            .filter(|e| e.deck == DeckId::A)
            .map(|e| e.value)
            .collect();
        assert_eq!(a_gains.first(), Some(&0.0));
        assert_eq!(a_gains.last(), Some(&1.0));
    }

    #[test]
    fn equal_power_sum_is_constant() {
        // Given the default pair's gain curves at both parities.
        for outgoing in [DeckId::A, DeckId::B] {
            let events = compile_transition(
                &long_crossfade(),
                window(44_100 * 30),
                120.0,
                44_100,
                outgoing,
            );
            // When pairing outgoing/incoming values by event time.
            let mut by_time: std::collections::BTreeMap<u64, [f32; 2]> = Default::default();
            for e in events.iter().filter(|e| curve_relevant(e)) {
                let idx = usize::from(e.deck == opposite(outgoing));
                by_time.entry(e.time.0).or_insert([0.0; 2])[idx] = e.value;
            }
            // Then power sums stay ~1 (equal-power crossfade).
            for (_, [a, b]) in by_time {
                let power = a.mul_add(a, b * b);
                assert!((power - 1.0).abs() < 0.02, "equal-power violated: {power}");
            }
        }
    }

    fn curve_relevant(e: &ControlEvent) -> bool {
        e.address == ParamAddress::Gain
    }

    fn opposite(deck: DeckId) -> DeckId {
        match deck {
            DeckId::A => DeckId::B,
            DeckId::B => DeckId::A,
        }
    }
}

#[cfg(test)]
mod golden_tests {
    use super::*;

    /// Golden: the default pair's first/last/mid event values per
    /// role at fixed bpm/rate/window.
    #[test]
    fn default_pair_golden_curve_values() {
        // Given the default pair compiled at 120 BPM over a 30 s window.
        let events = compile_transition(
            &long_crossfade(),
            TransitionWindow {
                start: SessionTime(0),
                end: SessionTime(44_100 * 30),
            },
            120.0,
            44_100,
            DeckId::A,
        );

        // Then the outgoing gain curve hits these checkpoints.
        let out: Vec<f32> = events
            .iter()
            .filter(|e| e.deck == DeckId::A)
            .map(|e| e.value)
            .collect();
        assert_eq!(out.first(), Some(&1.0));
        let mid = out[out.len() / 2];
        assert!(
            (mid - std::f64::consts::FRAC_1_SQRT_2 as f32).abs() < 0.01,
            "mid {mid}"
        );
        assert_eq!(*out.last().unwrap(), 0.0);

        // And there are 64*4+1 quarter-beat steps per curve.
        assert_eq!(out.len(), 64 * 4 + 1);
    }
}
