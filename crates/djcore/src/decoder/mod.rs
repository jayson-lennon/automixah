//! Trait-abstracted audio decoding.
//!
//! Ported from `harmonic-playlist` `feat/analysis/decoder` with path-based
//! decoding replaced by in-memory bytes decoding: automixah reads track
//! files from OPFS in a Web Worker, so bytes are the natural currency.

use std::collections::HashMap;
use std::sync::Arc;

use error_stack::{Report, ResultExt};
use wherror::Error;

pub mod symphonia;

pub use symphonia::SymphoniaDecoder;

/// Error returned when audio decoding fails.
#[derive(Debug, Error)]
#[error("audio decode error")]
pub struct DecodeError;

/// Decoded audio data: interleaved multi-channel normalized f32 samples.
#[derive(Debug, Clone)]
pub struct DecodeAudio {
    /// Interleaved samples in the range [-1.0, 1.0]
    /// (L, R, L, R, ... for stereo).
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u16,
}

impl DecodeAudio {
    /// Channel-averaged mono downmix (analysis input).
    #[must_use]
    pub fn to_mono(&self) -> Vec<f32> {
        let n = self.channels.max(1) as usize;
        if n == 1 {
            return self.samples.clone();
        }
        #[expect(clippy::cast_precision_loss, reason = "channel count is 1-2")]
        let scale = 1.0 / n as f32;
        self.samples
            .chunks(n)
            .map(|frame| frame.iter().sum::<f32>() * scale)
            .collect()
    }

    /// Frames (one per channel-count group).
    #[must_use]
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
}

/// Trait for audio format decoders.
///
/// Implementors decode audio files into mono normalized f32 samples
/// suitable for analysis. Each decoder declares which file extensions
/// it supports via [`supported_extensions`](AudioDecoder::supported_extensions).
pub trait AudioDecoder: Send + Sync {
    /// Returns the name of this decoder for debugging.
    fn name(&self) -> &'static str;

    /// Returns the file extensions this decoder supports (lowercase, without dot).
    fn supported_extensions(&self) -> &[&str];

    /// Decodes in-memory audio file bytes into mono normalized f32 samples.
    ///
    /// `extension` (lowercase, without dot) hints the container format.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes cannot be probed as a supported format,
    /// no audio track is found, or decoding fails.
    fn decode_bytes(
        &self,
        bytes: &[u8],
        extension: &str,
    ) -> Result<DecodeAudio, Report<DecodeError>>;
}

/// Registry that maps file extensions to [`AudioDecoder`] implementations.
///
/// Decoders are registered with their supported extensions. When decoding,
/// the registry looks up the appropriate decoder based on the file extension.
#[derive(Clone)]
pub struct DecoderRegistry {
    decoders: Arc<HashMap<String, Arc<dyn AudioDecoder>>>,
}

impl std::fmt::Debug for DecoderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecoderRegistry")
            .field("extensions", &self.decoders.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl DecoderRegistry {
    /// Creates an empty decoder registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            decoders: Arc::new(HashMap::new()),
        }
    }

    /// Creates a registry containing the [`SymphoniaDecoder`].
    #[must_use]
    pub fn with_symphonia() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(SymphoniaDecoder::new()));
        registry
    }

    /// Registers a decoder for all of its supported extensions.
    ///
    /// If multiple decoders support the same extension, the last one
    /// registered wins.
    pub fn register(&mut self, decoder: Arc<dyn AudioDecoder>) {
        let mut decoders = Arc::unwrap_or_clone(self.decoders.clone());
        for ext in decoder.supported_extensions() {
            decoders.insert((*ext).to_lowercase(), decoder.clone());
        }
        self.decoders = Arc::new(decoders);
    }

    /// Returns the decoder registered for the given file extension, if any.
    #[must_use]
    pub fn get(&self, extension: &str) -> Option<&Arc<dyn AudioDecoder>> {
        self.decoders.get(&extension.to_lowercase())
    }

    /// Decodes in-memory audio bytes by looking up the appropriate decoder
    /// based on file extension.
    ///
    /// # Errors
    ///
    /// Returns an error if no decoder is registered for the extension,
    /// or if the decoder itself fails.
    pub fn decode(
        &self,
        bytes: &[u8],
        extension: &str,
    ) -> Result<DecodeAudio, Report<DecodeError>> {
        let decoder = self.get(extension).ok_or_else(|| {
            Report::new(DecodeError).attach(format!(
                "no decoder registered for extension '.{extension}'"
            ))
        })?;

        decoder
            .decode_bytes(bytes, extension)
            .attach(format!("decoder '{}' failed", decoder.name()))
    }

    /// Returns all file extensions that have a registered decoder.
    #[must_use]
    pub fn supported_extensions(&self) -> Vec<String> {
        let mut exts: Vec<String> = self.decoders.keys().cloned().collect();
        exts.sort();
        exts
    }
}

impl Default for DecoderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake decoder echoing a fixed payload for any extension it handles.
    struct FakeDecoder {
        name: &'static str,
        exts: &'static [&'static str],
    }

    impl AudioDecoder for FakeDecoder {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supported_extensions(&self) -> &[&str] {
            self.exts
        }

        fn decode_bytes(
            &self,
            _bytes: &[u8],
            _extension: &str,
        ) -> Result<DecodeAudio, Report<DecodeError>> {
            Ok(DecodeAudio {
                samples: vec![0.5, -0.5],
                sample_rate: 48_000,
                channels: 1,
            })
        }
    }

    #[test]
    fn registry_resolves_decoder_by_extension() {
        // Given a registry with a fake mp3 decoder.
        let mut registry = DecoderRegistry::new();
        registry.register(Arc::new(FakeDecoder {
            name: "fake-mp3",
            exts: &["mp3"],
        }));

        // When decoding bytes tagged as mp3.
        let audio = registry.decode(&[], "mp3").expect("decode");

        // Then the fake payload comes back.
        assert_eq!(audio.samples, vec![0.5, -0.5]);
        assert_eq!(audio.sample_rate, 48_000);
    }

    #[test]
    fn registry_rejects_unknown_extension() {
        // Given an empty registry.
        let registry = DecoderRegistry::new();

        // When decoding bytes tagged with an unsupported extension.
        let result = registry.decode(&[], "xyz");

        // Then the error names the missing extension.
        assert!(result.is_err());
    }

    #[test]
    fn last_registered_decoder_wins_for_shared_extension() {
        // Given two decoders both claiming "wav".
        let mut registry = DecoderRegistry::new();
        registry.register(Arc::new(FakeDecoder {
            name: "first",
            exts: &["wav"],
        }));
        registry.register(Arc::new(FakeDecoder {
            name: "second",
            exts: &["wav"],
        }));

        // When resolving the wav decoder.
        let resolved = registry.get("wav").expect("decoder present");

        // Then the last registered decoder wins.
        assert_eq!(resolved.name(), "second");
    }

    #[test]
    fn extension_lookup_is_case_insensitive() {
        // Given a registry with a lowercase-registered decoder.
        let mut registry = DecoderRegistry::new();
        registry.register(Arc::new(FakeDecoder {
            name: "fake-mp3",
            exts: &["mp3"],
        }));

        // When resolving with uppercase input.
        let resolved = registry.get("MP3");

        // Then the decoder is found.
        assert!(resolved.is_some());
    }

    #[test]
    fn supported_extensions_are_sorted() {
        // Given a registry with multiple decoders.
        let mut registry = DecoderRegistry::new();
        registry.register(Arc::new(FakeDecoder {
            name: "a",
            exts: &["ogg", "aac"],
        }));
        registry.register(Arc::new(FakeDecoder {
            name: "b",
            exts: &["mp3"],
        }));

        // When listing supported extensions.
        let exts = registry.supported_extensions();

        // Then they are sorted.
        assert_eq!(exts, vec!["aac", "mp3", "ogg"]);
    }
}
