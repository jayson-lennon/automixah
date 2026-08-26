//! Instant preview playback: a deliberately dumb second player beside the
//! grid-editor deck.
//!
//! [`PreviewPlayer`] wraps an [`OutputEngine`] locked at 1× — no scrub
//! varispeed, no drag gestures, no grid. All transport *decisions* (toggle,
//! wrap-on-replay, auto-stop at the true end) live in the pure
//! [`PreviewTransport`], so the policy is unit-testable without an audio
//! device; the player only translates decisions into engine commands and
//! seek writes. One audible source: starting a preview pauses the deck and
//! vice versa (the applier owns that rule, this module just obeys it).

use std::sync::Arc;

use djcore::decoder::DecodeAudio;

use crate::audio::output::{OutputEngine, OutputEngineError, Playhead, ScrubCommand};

/// Fixed playback speed of the preview (1× source frames per second; the
/// engine folds source rate → device rate internally).
pub const PREVIEW_SPEED: f32 = 1.0;

/// Positions within this many frames of the end count as "arrived".
///
/// The scrub reader stops on the final frames rather than reporting an
/// exact end-of-stream event, so equality alone can miss the latch.
const END_EPSILON_FRAMES: f64 = 4.0;

/// Pure transport policy for one preview: playing/ended flags, the replay
/// wrap, and the derived engine command. No device, no locks — fully
/// deterministic.
#[derive(Debug, Clone)]
pub struct PreviewTransport {
    /// Track length in source frames; arrival here ends playback.
    source_frames: u64,
    playing: bool,
    /// Latched once playback reached the true end; cleared by any seek
    /// back into the track.
    ended: bool,
}

impl PreviewTransport {
    #[must_use]
    pub fn new(source_frames: u64) -> Self {
        Self {
            source_frames,
            playing: false,
            ended: false,
        }
    }

    /// Whether `position` (source frames) sits at the track end.
    ///
    /// f64 throughout: positions past 2²⁴ frames stay exact enough to
    /// distinguish end-adjacent frames on long tracks.
    #[must_use]
    pub fn at_end(&self, position: f64) -> bool {
        #[expect(
            clippy::cast_precision_loss,
            reason = "frame count bound to display f64"
        )]
        let total = self.source_frames as f64;
        position >= total - END_EPSILON_FRAMES
    }

    /// Toggles playback. Returns whether replay must rewind to frame 0
    /// first — any press of play while sitting at the true end restarts
    /// from the top, latched or merely parked there by a seek.
    pub fn toggle_play(&mut self, at_end: bool) -> bool {
        if self.playing {
            self.pause();
            return false;
        }
        let wraps = at_end;
        if wraps {
            self.ended = false;
        }
        self.playing = true;
        wraps
    }

    /// Stops playback, keeping the position (resume continues from it).
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Seeks to `frame` (clamped to `[0, source_frames]`); returns the
    /// clamped target. Seeking off the end clears the ended latch.
    #[must_use]
    pub fn seek_frame(&mut self, frame: u64) -> f64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "clamped display frame becomes a source position"
        )]
        let target = frame.min(self.source_frames) as f64;
        self.ended = self.at_end(target);
        target
    }

    /// Auto-stop check against the live position: latches `ended` and
    /// silences the transport once playback arrives at the true end.
    pub fn sync(&mut self, position: f64) {
        if self.playing && self.at_end(position) {
            self.pause();
            self.ended = true;
        }
    }

    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    #[must_use]
    pub fn has_ended(&self) -> bool {
        self.ended
    }

    /// The command the engine should hold right now: fixed 1×, playing or
    /// silent. Pure by construction — no locks are consulted.
    #[must_use]
    pub fn next_command(&self) -> ScrubCommand {
        ScrubCommand {
            speed: PREVIEW_SPEED,
            playing: self.playing,
        }
    }
}

/// Owns the preview's audio pipeline: engine + transport, driven by the
/// transport-bar UI. Lives only while a preview exists; dropping it drops
/// the stream.
pub struct PreviewPlayer {
    engine: OutputEngine,
    transport: PreviewTransport,
}

impl PreviewPlayer {
    /// Starts playing `audio` immediately on the default output device.
    ///
    /// Consumes the decoded audio — the engine thread holds the PCM, and
    /// the transport bar derives everything else from lengths, not
    /// samples.
    ///
    /// # Errors
    ///
    /// Returns an error when the output device cannot be opened or the
    /// stream fails to start; no player exists on failure.
    pub fn start(audio: DecodeAudio) -> Result<Self, error_stack::Report<OutputEngineError>> {
        #[expect(clippy::cast_precision_loss, reason = "frame count to display f64")]
        let source_frames = audio.frames() as f64;
        let sample_rate = audio.sample_rate;
        let pcm = Arc::new(audio.samples);

        let engine = {
            let channels = usize::from(audio.channels).max(1);
            OutputEngine::start(pcm, sample_rate, channels, 0.0)?
        };

        let mut transport = PreviewTransport::new(source_frames.round() as u64);
        transport.toggle_play(false);
        let player = Self { engine, transport };
        player.push_command();
        Ok(player)
    }

    /// Shared playhead for position reads and the seek handshake.
    #[must_use]
    pub fn playhead(&self) -> Arc<Playhead> {
        self.engine.playhead()
    }

    /// Current transport policy (flags consulted by the transport bar).
    #[must_use]
    pub fn transport(&self) -> &PreviewTransport {
        &self.transport
    }

    /// Whether sound is playing right now (not parked, not latched-ended).
    #[must_use]
    pub fn is_audible(&self) -> bool {
        self.transport.is_playing()
    }

    /// Positions the transport bar reads: current playhead position and
    /// track length, both in source frames.
    #[must_use]
    pub fn position_frames(&self) -> f64 {
        self.engine.playhead().position()
    }

    /// Track length in source frames.
    #[must_use]
    pub fn source_frames(&self) -> u64 {
        self.transport.source_frames
    }

    /// The loaded audio's sample rate (Hz), for display math.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.engine.sample_rate()
    }

    /// Play/pause; a replay after reaching the true end rewinds to 0.
    pub fn toggle_play(&mut self) {
        let position = *self.engine.playhead().position.read();
        let rewinds = self.transport.toggle_play(self.transport.at_end(position));
        if rewinds {
            self.seek_frame(0);
        }
        self.push_command();
    }

    /// Silences the preview (the solo-latch action; the deck takes over).
    pub fn pause(&mut self) {
        self.transport.pause();
        self.push_command();
    }

    /// Seeks via the engine handshake (pending-seek consumed by the audio
    /// callback); the immediate position write keeps the bar honest before
    /// the next callback lands.
    pub fn seek_frame(&mut self, frame: u64) {
        let target = self.transport.seek_frame(frame);
        let playhead = self.engine.playhead();
        *playhead.seek.write() = Some(target);
        *playhead.position.write() = target;
    }

    /// Per-frame upkeep: latches the auto-stop at the true end. Cheap —
    /// one lock-guarded read.
    pub fn sync(&mut self) {
        let position = *self.engine.playhead().position.read();
        let was_playing = self.transport.is_playing();
        self.transport.sync(position);
        if was_playing != self.transport.is_playing() {
            self.push_command();
        }
    }

    /// Publishes the transport's current command to the audio thread.
    fn push_command(&self) {
        *self.engine.command.lock() = self.transport.next_command();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_RATE: u32 = 44_100;
    const TRACK_FRAMES: u64 = 44_100 * 10;

    fn transport() -> PreviewTransport {
        PreviewTransport::new(TRACK_FRAMES)
    }

    // Given a freshly created transport.
    // When play is toggled.
    // Then it plays at the fixed 1× speed.
    #[test]
    fn toggle_on_a_fresh_transport_plays_at_fixed_speed() {
        let mut t = transport();

        t.toggle_play(false);

        let cmd = t.next_command();
        assert!(cmd.playing);
        assert_eq!(cmd.speed, PREVIEW_SPEED);
    }

    // Given a playing transport.
    // When play is toggled again.
    // Then it pauses without latching the end.
    #[test]
    fn toggle_twice_pauses_without_latching_end() {
        let mut t = transport();
        t.toggle_play(false);

        t.toggle_play(false);

        assert!(!t.is_playing());
        assert!(!t.has_ended(), "ordinary pause is not an ended latch");
    }

    // Given a transport whose playback reached the true end (latched).
    // When play is toggled.
    // Then it replays from the start, audibly.
    #[test]
    fn toggle_after_end_rewraps_to_zero_playing() {
        let mut t = transport();
        t.sync(TRACK_FRAMES as f64);

        let rewinds = t.toggle_play(t.at_end(TRACK_FRAMES as f64));

        assert!(rewinds, "wrap requested");
        assert!(t.is_playing());
        assert!(!t.has_ended());
        assert!(t.next_command().playing);
    }

    // Given a playing transport positioned just inside the final frames of
    // a long track (past 2²⁴, where f32 position math would freeze).
    // When synced.
    // Then the auto-stop latches `ended` and silences the command.
    #[test]
    fn sync_latches_auto_stop_at_true_end_f64() {
        // 48 kHz × 250 s ≈ 12 M frames, well past 2²⁴.
        let long_track_frames = u64::from(48_000_u32) * 250;
        let mut t = PreviewTransport::new(long_track_frames);
        t.toggle_play(false);
        let near_end = long_track_frames as f64 - END_EPSILON_FRAMES / 2.0;

        t.sync(near_end);

        assert!(t.has_ended());
        assert!(!t.is_playing());
        assert!(!t.next_command().playing);
    }

    // Given a paused transport positioned just inside the final frames.
    // When synced.
    // Then the latch does not fire — auto-stop only applies while playing.
    #[test]
    fn sync_does_not_latch_when_paused_near_end() {
        let long_track_frames = u64::from(48_000_u32) * 250;
        let mut t = PreviewTransport::new(long_track_frames);
        let near_end = long_track_frames as f64 - END_EPSILON_FRAMES / 2.0;

        t.sync(near_end);

        assert!(!t.has_ended());
        assert!(!t.is_playing());
    }

    // Given a playing transport mid-track.
    // When synced.
    // Then nothing changes — mid-track positions never trip the latch.
    #[test]
    fn sync_mid_track_keeps_playing() {
        let mut t = transport();
        t.toggle_play(false);

        t.sync((u64::from(FRAME_RATE) * 5) as f64);

        assert!(t.is_playing());
        assert!(!t.has_ended());
    }

    // Given seek targets under, within, and past the source range.
    // When seek_frame clamps them.
    // Then every target lands inside `[0, source_frames]`.
    #[rstest::rstest]
    #[case(0, 0.0)]
    #[case(u64::from(FRAME_RATE) * 5, (u64::from(FRAME_RATE) * 5) as f64)]
    #[case(TRACK_FRAMES, TRACK_FRAMES as f64)]
    #[case(u64::MAX, TRACK_FRAMES as f64)]
    fn seek_frames_clamp_into_source_range(#[case] requested: u64, #[case] expected: f64) {
        let mut t = transport();

        let target = t.seek_frame(requested);

        assert_eq!(target, expected);
    }

    // Given an ended transport.
    // When a seek moves back into the track.
    // Then the latch clears and a following toggle plays without a wrap.
    #[test]
    fn seek_back_from_end_clears_the_latch_and_resumes_cleanly() {
        let mut t = transport();
        t.toggle_play(false);
        t.sync(TRACK_FRAMES as f64);
        assert!(t.has_ended(), "precondition");

        let mid_track = t.seek_frame(u64::from(FRAME_RATE) * 3);
        let rewinds = t.toggle_play(t.at_end(mid_track));

        assert!(!t.has_ended());
        assert!(t.is_playing());
        assert!(!rewinds, "mid-track replay must not rewind");
    }

    // Given any transport state.
    // When commands are derived.
    // Then the speed is always the fixed 1× (no varispeed ever leaks in).
    #[test]
    fn next_command_speed_is_always_fixed_one_times() {
        let paused = transport();
        let mut playing = transport();
        playing.toggle_play(false);

        let speeds = [paused.next_command().speed, playing.next_command().speed];

        assert_eq!(speeds, [PREVIEW_SPEED, PREVIEW_SPEED]);
    }

    // Given two transports differing only in playback state.
    // When cloned.
    // Then each keeps its own flags (the policy stays value-semantics).
    #[test]
    fn clone_keeps_flags_independent() {
        let mut original = transport();
        original.toggle_play(false);

        let snapshot = original.clone();
        original.pause();

        assert!(snapshot.is_playing(), "clone unaffected by later pause");
        assert!(!original.is_playing());
    }
}
