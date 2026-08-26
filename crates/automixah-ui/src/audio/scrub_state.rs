//! Scrub state machine: space toggles 1× playback; dragging plays audio at
//! drag velocity (vinyl-style); releasing restores the pre-drag state.

use crate::audio::output::ScrubCommand;

/// Interaction state of the scrub engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubState {
    /// Silence, position frozen.
    Paused,
    /// 1× playback from the current position.
    Playing,
    /// Dragging: speed follows drag velocity; `remembered` is the state to
    /// restore on release.
    Dragging { remembered: Box<ScrubState> },
}

/// Converts UI events into `ScrubCommand`s.
///
/// Events carry the drag delta in *seconds of audio time* (already converted
/// from pixels) so the machine stays unit-agnostic.
#[derive(Debug, Clone)]
pub struct ScrubMachine {
    state: ScrubState,
    /// Base speed at 1×, in source-frames-per-source-frame (the engine
    /// rate-folds to the device after scrubbing; unit is 1.0).
    unit_speed: f32,
    /// Low-pass on drag speed to keep small jitter from chirping.
    smoothed_drag: f32,
}

impl ScrubMachine {
    #[must_use]
    pub fn new(unit_speed: f32) -> Self {
        Self {
            state: ScrubState::Paused,
            unit_speed,
            smoothed_drag: 0.0,
        }
    }

    /// Current state (for tests; the UI drives via commands).
    #[cfg(test)]
    #[must_use]
    pub fn state(&self) -> &ScrubState {
        &self.state
    }

    /// Space bar: toggles between `Paused` and `Playing` at 1×.
    pub fn toggle_play(&mut self) {
        self.state = match self.state {
            ScrubState::Paused | ScrubState::Dragging { .. } => ScrubState::Playing,
            ScrubState::Playing => ScrubState::Paused,
        };
        self.smoothed_drag = 0.0;
    }

    /// Silence the machine outright: cancels 1× playback and any active drag,
    /// remembering nothing to restore. Used when another player takes over
    /// the output — nothing may resume behind it, not even a released drag.
    pub fn pause(&mut self) {
        self.state = ScrubState::Paused;
        self.smoothed_drag = 0.0;
    }

    /// Pointer down on the waveform: remember the current state.
    pub fn drag_start(&mut self) {
        if matches!(self.state, ScrubState::Dragging { .. }) {
            return;
        }
        self.state = ScrubState::Dragging {
            remembered: Box::new(std::mem::replace(&mut self.state, ScrubState::Paused)),
        };
        self.smoothed_drag = 0.0;
    }

    /// Pointer move while down: `delta_seconds` is the audio-time delta since
    /// the previous frame; `frame_dt` the wall-clock frame duration.
    pub fn drag_move(&mut self, delta_seconds: f32, frame_dt: f32) {
        if !matches!(self.state, ScrubState::Dragging { .. }) {
            return;
        }
        let dt = frame_dt.max(1.0 / 240.0);
        let raw = delta_seconds / dt;
        // Exponential smoothing: reach ~90% of target in ~100 ms.
        let alpha = 1.0 - (-dt / 0.03).exp();
        self.smoothed_drag += (raw - self.smoothed_drag) * alpha;
    }

    /// Pointer up: restore the remembered state (play from here or pause).
    pub fn drag_end(&mut self) {
        if let ScrubState::Dragging { remembered } =
            std::mem::replace(&mut self.state, ScrubState::Paused)
        {
            self.state = *remembered;
        }
        self.smoothed_drag = 0.0;
    }

    /// Current audio command for the output engine.
    #[must_use]
    pub fn command(&self) -> ScrubCommand {
        match self.state {
            ScrubState::Paused => ScrubCommand {
                speed: self.unit_speed,
                playing: false,
            },
            ScrubState::Playing => ScrubCommand {
                speed: self.unit_speed,
                playing: true,
            },
            ScrubState::Dragging { .. } => ScrubCommand {
                speed: self.smoothed_drag.clamp(-8.0, 8.0) * self.unit_speed,
                playing: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: f32 = 0.91875; // 44.1k source on 48k device

    fn machine() -> ScrubMachine {
        ScrubMachine::new(UNIT)
    }

    /// (description, events, expected command playing, expected speed)
    fn cases() -> Vec<(&'static str, Vec<Event>, bool, f32)> {
        use Event::*;
        vec![
            ("idle emits silence at unit speed", vec![], false, UNIT),
            ("space starts 1x playback", vec![Space], true, UNIT),
            (
                "double space returns to pause",
                vec![Space, Space],
                false,
                UNIT,
            ),
            (
                "pause→drag→release restores pause",
                vec![DragStart, DragEnd],
                false,
                UNIT,
            ),
            (
                "play→drag→release resumes play",
                vec![Space, DragStart, DragEnd],
                true,
                UNIT,
            ),
            (
                "drag plays during drag even from pause",
                vec![DragStart],
                true,
                0.0, // smoothed start
            ),
            (
                "space during drag jumps to play",
                vec![DragStart, Space],
                true,
                UNIT,
            ),
        ]
    }

    enum Event {
        Space,
        DragStart,
        DragEnd,
    }

    // Given each table row.
    // When events are applied to a fresh machine.
    // Then the command matches the row.
    #[test]
    fn table_driven_transitions() {
        for (desc, events, playing, speed) in cases() {
            let mut m = machine();
            for e in events {
                match e {
                    Event::Space => m.toggle_play(),
                    Event::DragStart => m.drag_start(),
                    Event::DragEnd => m.drag_end(),
                }
            }
            let cmd = m.command();
            assert_eq!(cmd.playing, playing, "{desc}: playing");
            assert!(
                (cmd.speed - speed).abs() < 1e-4,
                "{desc}: speed {} vs {speed}",
                cmd.speed
            );
        }
    }

    // Given a paused machine with an active drag.
    // When the drag begins before it moves.
    // Then the command is already marked playing so the audio callback can
    // begin the scrub immediately.
    #[test]
    fn drag_start_command_is_playing() {
        let mut m = machine();
        m.drag_start();
        let command = m.command();
        assert!(command.playing);
        assert_eq!(command.speed, 0.0);
    }

    // Given a paused machine with an active drag.
    // When the drag velocity changes.
    // Then the command speed approaches +2 × unit (clamped), playing.
    #[test]
    fn drag_velocity_sets_speed() {
        let mut m = machine();
        m.drag_start();
        // Simulate 0.5 s of frames at 60 fps, moving 2.0 s/s.
        for _ in 0..30 {
            m.drag_move(2.0 / 60.0, 1.0 / 60.0);
        }
        let cmd = m.command();
        assert!(cmd.playing);
        assert!(
            cmd.speed > 1.0 * UNIT && cmd.speed <= 2.0 * UNIT,
            "speed {} should approach 2×unit",
            cmd.speed
        );
    }

    // Given an active drag moving fast.
    // When velocity exceeds ±8.
    // Then the command clamps to ±8 × unit.
    #[test]
    fn drag_speed_clamps() {
        let mut m = machine();
        m.drag_start();
        for _ in 0..90 {
            m.drag_move(20.0 / 60.0, 1.0 / 60.0);
        }
        assert!(
            (m.command().speed - 8.0 * UNIT).abs() < 0.05,
            "clamped high"
        );
        for _ in 0..90 {
            m.drag_move(-20.0 / 60.0, 1.0 / 60.0);
        }
        assert!(
            (m.command().speed + 8.0 * UNIT).abs() < 0.05,
            "clamped low: {}",
            m.command().speed
        );
    }

    // Given a playing machine.
    // When pause is requested.
    // Then the command is silence at unit speed.
    #[test]
    fn pause_silences_playing_machine() {
        let mut m = machine();
        m.toggle_play();

        m.pause();

        let cmd = m.command();
        assert!(!cmd.playing);
        assert_eq!(cmd.speed, UNIT);
    }

    // Given an already paused machine.
    // When pause is requested.
    // Then the machine stays paused.
    #[test]
    fn pause_is_idempotent_on_paused_machine() {
        let mut m = machine();

        m.pause();

        assert!(!m.command().playing);
    }

    // Given a playing machine with an active drag.
    // When pause is requested mid-drag.
    // Then releasing the drag afterwards restores silence — the takeover is
    // not undone by the drag ending, and no drag chirp leaks into the command.
    #[test]
    fn pause_during_drag_stays_silent_after_release() {
        let mut m = machine();
        m.toggle_play();
        m.drag_start();

        m.pause();

        m.drag_end();
        let cmd = m.command();
        assert!(!cmd.playing);
        assert_eq!(cmd.speed, UNIT);
    }
}
