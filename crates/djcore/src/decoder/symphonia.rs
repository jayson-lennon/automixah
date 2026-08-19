//! Symphonia-based audio decoder.
//!
//! Decodes MP3, FLAC, WAV, OGG, and AAC from in-memory bytes using the
//! symphonia crate. Ported from `harmonic-playlist`; the file-based entry
//! point was replaced with a bytes entry point for wasm/OPFS use.

use std::io::Cursor;

use error_stack::{Report, ResultExt};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use super::{AudioDecoder, DecodeAudio, DecodeError};

/// Audio decoder backed by the symphonia crate.
///
/// Supports MP3, FLAC, WAV, OGG, and AAC formats.
#[derive(Debug, Clone, Default)]
pub struct SymphoniaDecoder;

impl SymphoniaDecoder {
    /// Creates a new symphonia decoder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// File extensions supported by the symphonia decoder (lowercase, without dot).
///
/// `opus` requires the libopus adapter, which is native-only.
#[cfg(not(target_arch = "wasm32"))]
const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac", "opus", "m4a"];
#[cfg(target_arch = "wasm32")]
const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac", "m4a"];

/// Codec registry with every feature-enabled codec plus, on native
/// targets, the libopus adapter (symphonia has no first-party Opus
/// decoder). Built once; registry construction is not cheap.
#[cfg(not(target_arch = "wasm32"))]
fn codec_registry() -> &'static symphonia::core::codecs::CodecRegistry {
    use symphonia::core::codecs::CodecRegistry;
    static REGISTRY: std::sync::OnceLock<CodecRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
        registry
    })
}

/// Wasm has no libopus; the feature-enabled default registry applies.
#[cfg(target_arch = "wasm32")]
fn codec_registry() -> &'static symphonia::core::codecs::CodecRegistry {
    symphonia::default::get_codecs()
}

#[allow(clippy::cast_precision_loss)]
/// Interleaves up to the first two channels (mono repeats channel 0;
/// more than two channels downmix to stereo L/R pairs).
fn interleave_channels(channel_data: &[Vec<f32>], num_channels: usize, frames: usize) -> Vec<f32> {
    match num_channels {
        1 => channel_data[0].clone(),
        2 => {
            let mut out = Vec::with_capacity(frames * 2);
            for (l, r) in channel_data[0].iter().zip(&channel_data[1]) {
                out.push(*l);
                out.push(*r);
            }
            out
        }
        n => {
            // >2 channels: front L/R pair, or mono duplicated when only
            // one plane exists.
            let has_two = channel_data.len() >= 2;
            let mut out = Vec::with_capacity(frames * 2);
            if has_two {
                for (l, r) in channel_data[0].iter().zip(&channel_data[1]) {
                    out.push(*l);
                    out.push(*r);
                }
            } else {
                for l in &channel_data[0] {
                    out.push(*l);
                    out.push(*l);
                }
            }
            let _ = n;
            out
        }
    }
}

impl AudioDecoder for SymphoniaDecoder {
    fn name(&self) -> &'static str {
        "symphonia"
    }

    fn supported_extensions(&self) -> &[&str] {
        SUPPORTED_EXTENSIONS
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cast_precision_loss)]
    #[allow(clippy::cast_possible_truncation)]
    fn decode_bytes(
        &self,
        bytes: &[u8],
        extension: &str,
    ) -> Result<DecodeAudio, Report<DecodeError>> {
        let cursor = Cursor::new(bytes.to_vec());
        let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());

        let mut hint = Hint::new();
        hint.with_extension(extension);

        let format_opts = FormatOptions::default();
        let metadata_opts = MetadataOptions::default();
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &metadata_opts)
            .change_context(DecodeError)
            .attach("unsupported audio format")?;

        let mut format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| Report::new(DecodeError).attach("no audio track found in file"))?;

        let track_id = track.id;
        let codec_params = track.codec_params.clone();
        let sample_rate = codec_params.sample_rate.unwrap_or(44_100);

        let decoder_opts = DecoderOptions::default();
        let mut decoder = codec_registry()
            .make(&codec_params, &decoder_opts)
            .change_context(DecodeError)
            .attach("failed to create audio decoder")?;

        let mut all_samples: Vec<f32> = Vec::new();
        let mut channels_seen: usize = 0;

        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }

            let Ok(audio_data) = decoder.decode(&packet) else {
                continue;
            };

            let frames = audio_data.frames();
            let channels = audio_data.spec().channels.count();
            if channels_seen == 0 {
                channels_seen = channels;
            }

            match audio_data {
                AudioBufferRef::F32(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels).map(|c| buf.chan(c).to_vec()).collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::S16(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|&s| f32::from(s) / 32_768.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::S32(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|&s| s as f32 / 2_147_483_648.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::U8(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|&s| (f32::from(s) - 128.0) / 128.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::F64(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| buf.chan(c).iter().map(|&s| s as f32).collect())
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::U16(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|&s| (f32::from(s) - 32_768.0) / 32_768.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::U24(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|s| (s.0 as f32 - 8_388_608.0) / 8_388_608.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::U32(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|&s| (s as f32 - 2_147_483_648.0) / 2_147_483_648.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::S8(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| buf.chan(c).iter().map(|&s| f32::from(s) / 128.0).collect())
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
                AudioBufferRef::S24(buf) => {
                    let buf = buf.as_ref();
                    let ch: Vec<Vec<f32>> = (0..channels)
                        .map(|c| {
                            buf.chan(c)
                                .iter()
                                .map(|s| s.0 as f32 / 8_388_608.0)
                                .collect()
                        })
                        .collect();
                    all_samples.extend(interleave_channels(&ch, channels, frames));
                }
            }
        }

        let out_channels = u16::try_from(channels_seen).unwrap_or(2).clamp(1, 2);
        Ok(DecodeAudio {
            samples: all_samples,
            sample_rate,
            channels: out_channels,
        })
    }
}
