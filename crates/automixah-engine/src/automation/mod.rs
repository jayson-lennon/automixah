//! Data-driven automation: control-event timelines behind the
//! [`ControlSource`] trait.
//!
//! A [`ControlSource`] produces [`ControlEvent`]s up to a session
//! time. The v1 implementation, [`TimelineSource`], replays a
//! pre-generated timeline of events (a preset). A future
//! `MidiDeviceSource` implements the same trait and drives the
//! identical [`ControlBus`](crate::control::ControlBus) — that is the
//! MIDI-ready proof: the bus cannot tell the two apart.

pub mod presets;
pub mod selection;
pub mod transition_spec;

use crate::control::{ControlBus, ControlEvent};
use crate::timeline::types::SessionTime;

/// A producer of [`ControlEvent`]s bound to the session clock.
///
/// Sources are *pull-based*: the render engine asks for everything
/// due up to `until` and applies the returned events to the bus.
/// Sources are expected to be monotonic — once events for a time
/// range have been returned, polling an earlier range yields nothing.
pub trait ControlSource {
    /// Human-readable source name (debugging).
    fn name(&self) -> &str;

    /// Returns all events with `time <= until` not yet delivered.
    fn poll(&mut self, until: SessionTime) -> Vec<ControlEvent>;
}

/// Replays a fixed timeline of [`ControlEvent`]s in time order.
///
/// This is the v1 automation driver: preset data compiles to a
/// `Vec<ControlEvent>` (sampled at 1/4-beat resolution per the
/// presets spec) and a `TimelineSource` delivers it to the bus.
#[derive(Debug, Clone)]
pub struct TimelineSource {
    name: String,
    events: Vec<ControlEvent>,
    /// Index of the next undelivered event.
    cursor: usize,
}

impl TimelineSource {
    /// Builds a source from events; they are sorted by time
    /// (stable, so equal-time order is preserved).
    #[must_use]
    pub fn new(name: impl Into<String>, mut events: Vec<ControlEvent>) -> Self {
        events.sort_by_key(|e| e.time.0);
        Self {
            name: name.into(),
            events,
            cursor: 0,
        }
    }
}

impl ControlSource for TimelineSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn poll(&mut self, until: SessionTime) -> Vec<ControlEvent> {
        let due: Vec<ControlEvent> = self.events[self.cursor..]
            .iter()
            .copied()
            .take_while(|e| e.time <= until)
            .collect();
        let consumed = due.len();
        self.cursor += consumed;
        due
    }
}

/// A test/source-agnostic driver that applies a source to a bus over
/// time, collecting the bus state at each poll boundary.
///
/// Used by tests to prove source equivalence (timeline vs fake) via
/// identical bus evolution.
#[must_use]
pub fn drive_source(
    source: &mut dyn ControlSource,
    bus: &mut ControlBus,
    boundaries: &[SessionTime],
) -> Vec<ControlBus> {
    let mut snapshots = Vec::with_capacity(boundaries.len());
    for &until in boundaries {
        let events = source.poll(until);
        bus.apply_all(&events);
        snapshots.push(bus.clone());
    }
    snapshots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{DeckId, ParamAddress};

    fn gain_event(deck: DeckId, value: f32, time: u64) -> ControlEvent {
        ControlEvent {
            deck,
            address: ParamAddress::Gain,
            value,
            time: SessionTime(time),
        }
    }

    #[test]
    fn timeline_poll_returns_events_in_time_order() {
        // Given events supplied out of order.
        let mut source = TimelineSource::new(
            "test",
            vec![
                gain_event(DeckId::A, 0.8, 200),
                gain_event(DeckId::A, 0.9, 100),
            ],
        );

        // When polling past both.
        let events = source.poll(SessionTime(300));

        // Then they arrive sorted by time.
        assert_eq!(events.len(), 2);
        assert!(events[0].time <= events[1].time);
    }

    #[test]
    fn timeline_poll_is_exclusive_of_future_events() {
        // Given events at 100 and 200.
        let mut source = TimelineSource::new(
            "test",
            vec![
                gain_event(DeckId::A, 0.9, 100),
                gain_event(DeckId::A, 0.8, 200),
            ],
        );

        // When polling to 150.
        let events = source.poll(SessionTime(150));

        // Then only the first event is due.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].time, SessionTime(100));
    }

    #[test]
    fn timeline_poll_is_non_repeating() {
        // Given a consumed source.
        let mut source = TimelineSource::new("test", vec![gain_event(DeckId::A, 0.9, 100)]);
        assert_eq!(source.poll(SessionTime(150)).len(), 1);

        // When polling the same range again.
        let events = source.poll(SessionTime(150));

        // Then nothing new is delivered.
        assert!(events.is_empty());
    }

    #[test]
    fn drive_source_snapshots_bus_evolution() {
        // Given a timeline fading B in.
        let mut source = TimelineSource::new(
            "fade",
            vec![
                gain_event(DeckId::B, 0.25, 100),
                gain_event(DeckId::B, 0.75, 200),
                gain_event(DeckId::B, 1.0, 300),
            ],
        );
        let mut bus = ControlBus::new();

        // When driving at each boundary.
        let snapshots = drive_source(
            &mut source,
            &mut bus,
            &[SessionTime(100), SessionTime(200), SessionTime(300)],
        );

        // Then each snapshot reflects the events due by then.
        assert_eq!(snapshots.len(), 3);
        assert!((snapshots[0].get(DeckId::B, ParamAddress::Gain) - 0.25).abs() < f32::EPSILON);
        assert!((snapshots[1].get(DeckId::B, ParamAddress::Gain) - 0.75).abs() < f32::EPSILON);
        assert!((snapshots[2].get(DeckId::B, ParamAddress::Gain) - 1.0).abs() < f32::EPSILON);
    }

    /// A minimal hand-written source (the fake-MIDI stand-in).
    struct FakeSource {
        events: Vec<ControlEvent>,
        cursor: usize,
    }

    impl ControlSource for FakeSource {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn poll(&mut self, until: SessionTime) -> Vec<ControlEvent> {
            let mut due = Vec::new();
            while self.cursor < self.events.len() && self.events[self.cursor].time <= until {
                due.push(self.events[self.cursor]);
                self.cursor += 1;
            }
            due
        }
    }

    #[test]
    fn fake_and_timeline_sources_drive_identical_bus_states() {
        // Given the same events behind a FakeSource and a TimelineSource.
        let events = vec![
            gain_event(DeckId::A, 0.9, 100),
            gain_event(DeckId::B, 0.1, 150),
            gain_event(DeckId::A, 0.5, 200),
            gain_event(DeckId::B, 0.9, 250),
        ];
        let mut timeline = TimelineSource::new("t", events.clone());
        let mut fake = FakeSource { events, cursor: 0 };
        let boundaries: Vec<SessionTime> = [100, 150, 200, 250, 300]
            .iter()
            .map(|&t| SessionTime(t))
            .collect();

        // When driving both from fresh buses.
        let timeline_states = drive_source(&mut timeline, &mut ControlBus::new(), &boundaries);
        let fake_states = drive_source(&mut fake, &mut ControlBus::new(), &boundaries);

        // Then the bus evolution is identical at every boundary.
        for (i, (tl, fk)) in timeline_states.iter().zip(&fake_states).enumerate() {
            for deck in [DeckId::A, DeckId::B] {
                for address in crate::control::ALL_ADDRESSES {
                    assert!(
                        (tl.get(deck, address) - fk.get(deck, address)).abs() < f32::EPSILON,
                        "bus state diverged at boundary {i} for {deck:?}/{address:?}"
                    );
                }
            }
        }
    }
}
