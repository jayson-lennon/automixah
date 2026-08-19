//! cpal output stream + device-rate fold.
//!
//! The scrub reader runs at the source rate; the device usually wants another
//! rate (e.g. 44.1 kHz track → 48 kHz DAC). `RateFolder` resamples the scrub
//! output with linear interpolation — fine for audition playback. Output is
//! −6 dB scaled and soft-clipped (tanh) so drag bursts never hard-clip.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::{Mutex, RwLock};

use super::scrub::ScrubCore;

/// Shared playhead: the audio thread writes position; the UI reads it.
#[allow(dead_code, reason = "wired to the scrub state machine next task")]
pub struct Playhead {
    /// Current read position in source frames.
    pub position: RwLock<f32>,
    /// Requested position jump (UI sets Some(frame); audio consumes it).
    pub seek: RwLock<Option<f32>>,
    /// Effective playback speed (source frames per second) at the last
    /// callback; the UI extrapolates position between callbacks.
    pub speed: RwLock<f32>,
}

/// Consumes a pending UI seek by rebuilding the scrub reader at the
/// requested frame. Runs before any position writeback in the callback
/// so a paused stream still lands on the sought position instead of
/// clobbering it with the stale scrub position.
fn apply_pending_seek(playhead: &Playhead, scrub: &mut ScrubCore, channels: usize) {
    if let Some(frame) = playhead.seek.write().take() {
        *scrub = ScrubCore::new(channels, frame);
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "wired to the scrub state machine next task")
)]
impl Playhead {
    #[must_use]
    pub fn new() -> Self {
        Self {
            position: RwLock::new(0.0),
            seek: RwLock::new(None),
            speed: RwLock::new(0.0),
        }
    }
}

impl Default for Playhead {
    fn default() -> Self {
        Self::new()
    }
}

/// Folds interleaved audio from `in_channels` to `out_channels`, one
/// output frame per input frame: mono duplicates to every output channel,
/// equal counts pass through, and more-in-than-out downmixes to the
/// first output channel (front pair for stereo devices).
fn fold_channels(input: &[f32], in_channels: usize, out: &mut [f32], out_channels: usize) {
    let in_frames = input.len() / in_channels.max(1);
    let out_frames = out.len() / out_channels.max(1);
    for f in 0..out_frames.min(in_frames) {
        for oc in 0..out_channels {
            let v = if in_channels == 1 {
                input[f]
            } else {
                let c = oc.min(in_channels - 1);
                input[f * in_channels + c]
            };
            out[f * out_channels + oc] = v;
        }
    }
}

/// Linear-interpolating resampler between distinct rates.
///
/// Holds one source frame of history; produces `ratio` device frames per
/// source frame at 1× speed.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by OutputEngine::start, wired to UI next task"
    )
)]
pub struct RateFolder {
    channels: usize,
    /// Device rate / source rate.
    ratio: f32,
    /// Fractional position within the source-rate stream [0, 1).
    phase: f32,
    /// Last emitted source frame (per channel), for interpolation.
    history: Vec<f32>,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed by OutputEngine::start, wired to UI next task"
    )
)]
impl RateFolder {
    #[must_use]
    pub fn new(channels: usize, source_rate: u32, device_rate: u32) -> Self {
        Self {
            channels: channels.max(1),
            #[expect(clippy::cast_precision_loss, reason = "rates fit f32 exactly")]
            ratio: device_rate.max(1) as f32 / source_rate.max(1) as f32,
            phase: 0.0,
            history: vec![0.0; channels.max(1)],
        }
    }

    /// The speed multiplier to apply so 1× playback consumes source frames at
    /// the correct device-time rate.
    #[must_use]
    pub fn speed_scale(&self) -> f32 {
        1.0 / self.ratio
    }

    /// Folds `input` (source-rate interleaved) into `out` (device-rate),
    /// returning the number of input frames consumed.
    pub fn fold(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        let ch = self.channels;
        let in_frames = input.len() / ch;
        let out_frames = out.len() / ch;
        let mut consumed = 0;

        for of in 0..out_frames {
            // Interpolate between history and the next unconsumed input frame.
            while self.phase >= 1.0 && consumed < in_frames {
                for c in 0..ch {
                    self.history[c] = input[consumed * ch + c];
                }
                consumed += 1;
                self.phase -= 1.0;
            }
            let next = if consumed < in_frames {
                (0..ch)
                    .map(|c| input[consumed * ch + c])
                    .collect::<Vec<_>>()
            } else {
                self.history.clone()
            };
            for c in 0..ch {
                let a = self.history[c];
                let b = next[c];
                out[of * ch + c] = a + (b - a) * self.phase;
            }
            self.phase += 1.0 / self.ratio.max(0.0001);
        }
        consumed
    }
}

/// −6 dB then soft clip (tanh): keeps drag bursts tame without hard edges.
#[allow(dead_code, reason = "used in the audio callback; UI wiring next task")]
pub fn shape(sample: f32) -> f32 {
    (sample * 0.5).tanh()
}

/// Error starting the audio output pipeline.
#[derive(Debug, wherror::Error)]
#[error(debug)]
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "constructed only via OutputEngine::start failures"
    )
)]
pub struct OutputEngineError;

/// Owns the cpal stream and the shared scrub state.
#[allow(dead_code, reason = "started by the app once a track loads")]
pub struct OutputEngine {
    playhead: Arc<Playhead>,
    /// Scrub commands: (speed, playing).
    pub command: Arc<Mutex<ScrubCommand>>,
    _stream: cpal::Stream,
}

/// What the audio thread should do this callback.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code, reason = "written by the scrub state machine next task")]
pub struct ScrubCommand {
    /// Speed in source-frames per device-frame (already rate-folded).
    pub speed: f32,
    /// False → emit silence (paused), position frozen.
    pub playing: bool,
}

#[allow(dead_code, reason = "started by the app once a track loads")]
impl OutputEngine {
    /// Builds the engine for `pcm` (interleaved stereo at `source_rate`).
    ///
    /// # Errors
    ///
    /// Returns an error if no output device exists, its default config
    /// cannot be read, or the stream fails to build or start.
    pub fn start(
        pcm: Arc<Vec<f32>>,
        source_rate: u32,
        channels: usize,
        start_frame: f32,
    ) -> Result<Self, error_stack::Report<OutputEngineError>> {
        use error_stack::ResultExt as _;
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            error_stack::Report::new(OutputEngineError).attach("no default output device")
        })?;
        let supported = device
            .default_output_config()
            .change_context(OutputEngineError)
            .attach("read default output config")?;
        let config: cpal::StreamConfig = supported.into();
        let device_rate = config.sample_rate.0;
        let device_channels = usize::from(config.channels);

        let playhead = Arc::new(Playhead::new());
        *playhead.position.write() = start_frame;
        let command = Arc::new(Mutex::new(ScrubCommand {
            speed: 1.0,
            playing: false,
        }));

        // Scrub reads at source channels; fold converts to the device's.
        let mut scrub = ScrubCore::new(channels, start_frame);
        let mut folder = RateFolder::new(device_channels, source_rate, device_rate);
        // Scratch buffers, grown on demand and reused across callbacks so
        // the real-time path does not allocate.
        let mut src = Vec::<f32>::new();
        let mut chan_folded = Vec::<f32>::new();
        let mut folded = Vec::<f32>::new();

        let cb_playhead = Arc::clone(&playhead);
        let cb_command = Arc::clone(&command);
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _| {
                    let cmd = *cb_command.lock();
                    apply_pending_seek(&cb_playhead, &mut scrub, channels);
                    if !cmd.playing {
                        out.fill(0.0);
                        *cb_playhead.position.write() = scrub.position();
                        *cb_playhead.speed.write() = 0.0;
                        return;
                    }
                    scrub.set_speed(cmd.speed);

                    // Fold needs one source frame per device frame scaled by
                    // source/device (scrub speed spans more source *time*,
                    // not more frames), plus margin for interpolation.
                    let device_frames = out.len() / device_channels.max(1);
                    #[expect(clippy::cast_precision_loss, reason = "rates fit f32 exactly")]
                    let needed = (device_frames as f32 * source_rate as f32 / device_rate as f32)
                        .ceil() as usize
                        + 4;
                    src.resize(needed * channels, 0.0);
                    scrub.read(&pcm, &mut src);
                    *cb_playhead.position.write() = scrub.position();
                    *cb_playhead.speed.write() = cmd.speed * source_rate as f32;

                    // Source channels → device channels, then source rate →
                    // device rate, shape, write.
                    chan_folded.resize(src.len() / channels.max(1) * device_channels, 0.0);
                    fold_channels(&src, channels, &mut chan_folded, device_channels);
                    folded.resize(out.len(), 0.0);
                    let _ = folder.fold(&chan_folded, &mut folded);
                    for (o, s) in out.iter_mut().zip(folded.iter()) {
                        *o = shape(*s);
                    }
                },
                |err| eprintln!("audio stream error: {err}"),
                None,
            )
            .change_context(OutputEngineError)
            .attach("build output stream")?;
        stream
            .play()
            .change_context(OutputEngineError)
            .attach("start output stream")?;

        Ok(Self {
            playhead,
            command,
            _stream: stream,
        })
    }

    /// Shared playhead handle for UI reads (position) and writes (seek).
    #[must_use]
    pub fn playhead(&self) -> Arc<Playhead> {
        Arc::clone(&self.playhead)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a 441 Hz sine at 44.1 kHz folded to 48 kHz.
    // When 1 s is rendered at 1x.
    // Then the tone is preserved (~441 Hz) within tolerance.
    #[test]
    fn rate_fold_preserves_tone_44k_to_48k() {
        use std::f32::consts::TAU;
        let rate = 44_100.0;
        let hz = 441.0;
        let src: Vec<f32> = (0..(rate as usize))
            .flat_map(|i| {
                let v = (TAU * hz * i as f32 / rate).sin();
                [v, v]
            })
            .collect();

        let mut folder = RateFolder::new(2, 44_100, 48_000);
        let mut out = vec![0.0_f32; 48_000 * 2];
        let consumed = folder.fold(&src, &mut out);
        assert!(consumed > 44_000, "consumed {consumed}");

        // Zero-crossing frequency check on mono downmix.
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
            panic!("no crossings");
        };
        let measured = 48_000.0 * (l - f) as f32 / mono.len() as f32;
        assert!(
            (measured - hz).abs() < 12.0,
            "measured {measured} Hz vs {hz}"
        );
    }

    // Given a 440 Hz sine at 48 kHz folded down to 44.1 kHz
    // (the opus case that previously crackled: the fold ran dry
    // partway through every callback and repeated its last frame).
    // When 1 s is rendered at 1x.
    // Then the fold never runs dry: consumption matches the needed
    // source frames and the tone is preserved.
    #[test]
    fn rate_fold_48k_to_44k1_never_runs_dry() {
        use std::f32::consts::TAU;
        let src_rate = 48_000.0f32;
        let device_rate = 44_100u32;
        let hz = 440.0;
        #[expect(clippy::cast_possible_truncation, reason = "fixture size")]
        let src: Vec<f32> = (0..src_rate as usize)
            .flat_map(|i| {
                let v = (TAU * hz * i as f32 / src_rate).sin();
                [v, v]
            })
            .collect();

        let mut folder = RateFolder::new(2, 48_000, device_rate);
        let mut out = vec![0.0_f32; device_rate as usize * 2];
        let needed = out.len() / 2 * 48_000 / device_rate as usize;
        let consumed = folder.fold(&src, &mut out);
        assert!(
            (consumed as i64 - needed as i64).abs() <= 2,
            "consumed {consumed}, needed ~{needed} — dry fold repeats frames"
        );

        let mono: Vec<f32> = out.iter().step_by(2).copied().collect();
        let measured = dominant_hz_zero_crossings(&mono, device_rate as f32);
        assert!(
            (measured - hz).abs() < 12.0,
            "measured {measured} Hz vs {hz} — staircase repeats show as noise"
        );
    }

    // Given mono source audio on a stereo device.
    // When channel-folded.
    // Then every frame duplicates the mono sample to both channels.
    #[test]
    fn channel_fold_duplicates_mono_to_stereo() {
        let src = vec![0.1_f32, 0.2, 0.3, 0.4];
        let mut out = vec![0.0_f32; src.len() * 2];
        fold_channels(&src, 1, &mut out, 2);
        assert_eq!(out, vec![0.1, 0.1, 0.2, 0.2, 0.3, 0.3, 0.4, 0.4]);
    }

    // Given stereo source on a stereo device.
    // When channel-folded.
    // Then samples pass through unchanged.
    #[test]
    fn channel_fold_stereo_is_identity() {
        let src = vec![0.1_f32, -0.1, 0.2, -0.2];
        let mut out = vec![0.0_f32; src.len()];
        fold_channels(&src, 2, &mut out, 2);
        assert_eq!(out, src);
    }

    /// Average crossing-based frequency of a mono buffer.
    fn dominant_hz_zero_crossings(mono: &[f32], rate: f32) -> f32 {
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

    // Given samples beyond full scale.
    // When shaped.
    // Then output is bounded to (-1, 1) and monotonic-ish (no hard clip).
    #[test]
    fn shape_soft_clips() {
        assert!(shape(4.0) < 1.0 && shape(4.0) > 0.9);
        assert!(shape(-4.0) > -1.0 && shape(-4.0) < -0.9);
        assert!((shape(0.1) - 0.05).abs() < 0.01, "−6 dB headroom");
    }

    // Given a 1x speed command at 48k for a 44.1k source.
    // When speed_scale is applied.
    // Then scrub advances at source frames per device second correctly.
    #[test]
    fn speed_scale_compensates_rates() {
        let folder = RateFolder::new(2, 44_100, 48_000);
        let scale = folder.speed_scale();
        // 48k device frames × scale = 44.1k source frames per second.
        assert!((48_000.0 * scale - 44_100.0).abs() < 1.0, "scale {scale}");
    }

    // Given a paused stream with a pending UI seek to a nonzero frame.
    // When the paused callback path runs.
    // Then the seek is consumed and the reported position matches it
    // (ordering: seek consumption precedes the paused position write).
    #[test]
    fn paused_seek_updates_position() {
        let playhead = Playhead::new();
        let mut scrub = ScrubCore::new(2, 0.0);
        // The UI writes both, like the click-to-seek handler.
        *playhead.seek.write() = Some(12_345.0);
        *playhead.position.write() = 12_345.0;

        // Paused callback: consume first, then write back position.
        apply_pending_seek(&playhead, &mut scrub, 2);
        let reported = scrub.position();

        assert_eq!(reported, 12_345.0, "seek frame applied");
        assert!(playhead.seek.read().is_none(), "seek consumed");
    }
}
