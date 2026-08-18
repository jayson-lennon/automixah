//! The addressed, MIDI-shaped control bus.
//!
//! The mixer's entire parameter surface is exposed as
//! (deck, address) → normalized value. Automations, presets, and (in
//! the future) MIDI devices are all [`ControlSource`]s that emit
//! [`ControlEvent`]s onto the bus; the render engine consumes bus
//! state. This is the seam that makes hardware control a drop-in: a
//! MIDI CC maps directly to a (deck, address) pair with a value in
//! `[0, 1]`.
//!
//! Normalized semantics:
//!
//! - `Gain`: `[0, 1]` linear attenuation (1.0 = unity).
//! - `EqLow`/`EqMid`/`EqHigh`: `[0, 1]`, 0.5 = unity (0 dB);
//!   endpoints ±12 dB (shelf/peak gains).
//! - `HpfCutoff`/`LpfCutoff`: `[0, 1]` maps exponentially
//!   20 Hz–20 kHz; 0.0 HPF = bypass (20 Hz), 1.0 LPF = bypass
//!   (20 kHz).

use crate::timeline::types::SessionTime;

/// Which deck a parameter belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckId {
    /// Deck A (outgoing during a transition).
    A,
    /// Deck B (incoming during a transition).
    B,
}

/// An addressable mixer parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamAddress {
    /// Linear gain, `[0, 1]`.
    Gain,
    /// Low shelf gain, `[0, 1]` → ±12 dB around 0.5.
    EqLow,
    /// Mid peak gain, `[0, 1]` → ±12 dB around 0.5.
    EqMid,
    /// High shelf gain, `[0, 1]` → ±12 dB around 0.5.
    EqHigh,
    /// High-pass cutoff, `[0, 1]` → 20 Hz–20 kHz exponential.
    HpfCutoff,
    /// Low-pass cutoff, `[0, 1]` → 20 Hz–20 kHz exponential.
    LpfCutoff,
}

/// The default value for an address (unity / bypass).
#[must_use]
pub fn default_value(address: ParamAddress) -> f32 {
    match address {
        ParamAddress::Gain | ParamAddress::LpfCutoff => 1.0,
        // HPF neutral is "off" (20 Hz) so a neutral bus bypasses it.
        ParamAddress::HpfCutoff => 0.0,
        ParamAddress::EqLow | ParamAddress::EqMid | ParamAddress::EqHigh => 0.5,
    }
}

/// A single control event: set `address` on `deck` to `value` at
/// `time` (session samples).
///
/// Shaped like a MIDI CC message (channel=deck, controller=address,
/// value in [0,1] with 7-bit resolution available via
/// [`ControlEvent::from_midi_cc`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlEvent {
    /// Target deck.
    pub deck: DeckId,
    /// Target parameter.
    pub address: ParamAddress,
    /// Normalized value in `[0, 1]`.
    pub value: f32,
    /// Session sample time the value takes effect.
    pub time: SessionTime,
}

impl ControlEvent {
    /// Builds a control event from a 7-bit MIDI CC value.
    ///
    /// `0..=127` maps linearly to `[0, 1]`.
    #[must_use]
    pub fn from_midi_cc(deck: DeckId, address: ParamAddress, cc: u8, time: SessionTime) -> Self {
        Self {
            deck,
            address,
            value: f32::from(cc) / 127.0,
            time,
        }
    }
}

/// The live parameter state of both decks.
///
/// Values are normalized per [`ParamAddress`] semantics. The bus is
/// the *only* way the render engine learns parameter changes.
#[derive(Debug, Clone)]
pub struct ControlBus {
    /// Per-deck parameter values.
    values: [f32; 12],
}

impl Default for ControlBus {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlBus {
    /// A bus with all parameters at their defaults (unity/bypass).
    #[must_use]
    pub fn new() -> Self {
        let mut values = [0.0; 12];
        for deck in [DeckId::A, DeckId::B] {
            for address in ALL_ADDRESSES {
                values[Self::index(deck, address)] = default_value(address);
            }
        }
        Self { values }
    }

    /// Flat index into `values` for a (deck, address) pair.
    fn index(deck: DeckId, address: ParamAddress) -> usize {
        let deck_base = match deck {
            DeckId::A => 0,
            DeckId::B => 6,
        };
        let addr = match address {
            ParamAddress::Gain => 0,
            ParamAddress::EqLow => 1,
            ParamAddress::EqMid => 2,
            ParamAddress::EqHigh => 3,
            ParamAddress::HpfCutoff => 4,
            ParamAddress::LpfCutoff => 5,
        };
        deck_base + addr
    }

    /// Reads the current value for (deck, address).
    #[must_use]
    pub fn get(&self, deck: DeckId, address: ParamAddress) -> f32 {
        self.values[Self::index(deck, address)]
    }

    /// Sets a value directly (clamped) — the non-event form of [`ControlBus::apply`].
    pub fn set(&mut self, deck: DeckId, address: ParamAddress, value: f32) {
        self.values[Self::index(deck, address)] = value.clamp(0.0, 1.0);
    }

    /// Applies one event; clamps the value into `[0, 1]`.
    pub fn apply(&mut self, event: ControlEvent) {
        self.values[Self::index(event.deck, event.address)] = event.value.clamp(0.0, 1.0);
    }

    /// Applies a batch of events in order.
    pub fn apply_all(&mut self, events: &[ControlEvent]) {
        for event in events {
            self.apply(*event);
        }
    }
}

/// All addresses, in bus-index order.
pub const ALL_ADDRESSES: [ParamAddress; 6] = [
    ParamAddress::Gain,
    ParamAddress::EqLow,
    ParamAddress::EqMid,
    ParamAddress::EqHigh,
    ParamAddress::HpfCutoff,
    ParamAddress::LpfCutoff,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_bus_is_at_unity_and_bypass() {
        // Given a fresh bus.
        let bus = ControlBus::new();

        // Then gains are unity and EQs are at 0.5 (0 dB).
        assert!((bus.get(DeckId::A, ParamAddress::Gain) - 1.0).abs() < f32::EPSILON);
        assert!((bus.get(DeckId::B, ParamAddress::Gain) - 1.0).abs() < f32::EPSILON);
        assert!((bus.get(DeckId::A, ParamAddress::EqLow) - 0.5).abs() < f32::EPSILON);
        assert!((bus.get(DeckId::B, ParamAddress::LpfCutoff) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn apply_sets_value_on_target_deck_only() {
        // Given a bus and an event targeting deck B's gain.
        let mut bus = ControlBus::new();
        let event = ControlEvent {
            deck: DeckId::B,
            address: ParamAddress::Gain,
            value: 0.25,
            time: SessionTime(0),
        };

        // When applying.
        bus.apply(event);

        // Then B's gain changed and A's is untouched.
        assert!((bus.get(DeckId::B, ParamAddress::Gain) - 0.25).abs() < f32::EPSILON);
        assert!((bus.get(DeckId::A, ParamAddress::Gain) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn apply_clamps_out_of_range_values() {
        // Given a bus and events with out-of-range values.
        let mut bus = ControlBus::new();

        // When applying.
        bus.apply_all(&[
            ControlEvent {
                deck: DeckId::A,
                address: ParamAddress::Gain,
                value: 2.0,
                time: SessionTime(0),
            },
            ControlEvent {
                deck: DeckId::A,
                address: ParamAddress::EqMid,
                value: -1.0,
                time: SessionTime(0),
            },
        ]);

        // Then values are clamped.
        assert!((bus.get(DeckId::A, ParamAddress::Gain) - 1.0).abs() < f32::EPSILON);
        assert!((bus.get(DeckId::A, ParamAddress::EqMid) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn midi_cc_maps_linearly_to_normalized() {
        // Given CC values 0, 64, 127.
        // When converting.
        let zero = ControlEvent::from_midi_cc(DeckId::A, ParamAddress::Gain, 0, SessionTime(0));
        let mid = ControlEvent::from_midi_cc(DeckId::A, ParamAddress::Gain, 64, SessionTime(0));
        let max = ControlEvent::from_midi_cc(DeckId::A, ParamAddress::Gain, 127, SessionTime(0));

        // Then they map to 0, ~0.504, 1.
        assert!((zero.value - 0.0).abs() < f32::EPSILON);
        assert!((mid.value - 64.0 / 127.0).abs() < f32::EPSILON);
        assert!((max.value - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn later_events_win_in_batch() {
        // Given two events for the same parameter.
        let mut bus = ControlBus::new();
        let events = [
            ControlEvent {
                deck: DeckId::B,
                address: ParamAddress::EqHigh,
                value: 0.1,
                time: SessionTime(100),
            },
            ControlEvent {
                deck: DeckId::B,
                address: ParamAddress::EqHigh,
                value: 0.9,
                time: SessionTime(200),
            },
        ];

        // When applying in order.
        bus.apply_all(&events);

        // Then the final state reflects the last event.
        assert!((bus.get(DeckId::B, ParamAddress::EqHigh) - 0.9).abs() < f32::EPSILON);
    }
}
