//! Container metadata probing: title/artist tags without decoding audio.
//!
//! A light probe of the container header — no packets are decoded. Used by
//! the playlist queue to label tracks immediately after hashing; the
//! filename fallback fills in when tags are absent.

use std::io::Cursor;

use error_stack::{Report, ResultExt};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;
use symphonia::core::units::TimeBase;

use super::DecodeError;

/// Container tags for a track (any field may be absent).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackTags {
    /// Track title tag.
    pub title: Option<String>,
    /// Artist tag.
    pub artist: Option<String>,
    /// Duration in seconds, derived from the codec parameters.
    pub duration_seconds: Option<f32>,
}

/// Probes container metadata without decoding audio packets.
///
/// Mirrors [`super::SymphoniaDecoder`]'s probe prologue (same hint +
/// metadata options) but never touches the packet loop. Duration comes
/// from the default track's `codec_params` — approximate is acceptable.
///
/// # Errors
///
/// Returns an error if the container cannot be probed or has no track.
pub fn probe_metadata(bytes: &[u8], extension: &str) -> Result<TrackTags, Report<DecodeError>> {
    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    hint.with_extension(extension);

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .change_context(DecodeError)
        .attach("unsupported audio format")?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| Report::new(DecodeError).attach("no audio track found in file"))?;

    let mut tags = TrackTags {
        duration_seconds: duration_from_params(
            track.codec_params.time_base,
            track.codec_params.n_frames,
        ),
        ..TrackTags::default()
    };
    // The current revision holds the merged view; earlier revisions are
    // superseded.
    if let Some(revision) = format.metadata().current() {
        for tag in revision.tags() {
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) => {
                    tags.title = Some(tag.value.to_string());
                }
                Some(StandardTagKey::Artist) => {
                    tags.artist = Some(tag.value.to_string());
                }
                _ => {}
            }
        }
    }
    Ok(tags)
}

/// Duration in seconds from the codec's frame count and time base.
fn duration_from_params(time_base: Option<TimeBase>, n_frames: Option<u64>) -> Option<f32> {
    let tb = time_base?;
    let frames = n_frames?;
    #[expect(clippy::cast_precision_loss, reason = "frame count to f64")]
    let seconds = frames as f64 * f64::from(tb.numer) / f64::from(tb.denom);
    #[expect(clippy::cast_possible_truncation, reason = "duration seconds")]
    Some(seconds as f32)
}

/// Splits a file stem into `(artist, title)` on the first `" - "`.
///
/// `"Arome - Hands Up!"` → `("Arome", "Hands Up!")`; no separator means
/// the whole stem is the title and the artist is empty.
#[must_use]
pub fn filename_fallback(stem: &str) -> (String, String) {
    match stem.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_owned(), title.trim().to_owned()),
        None => (String::new(), stem.trim().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a stem with an " - " separator.
    // When split by the fallback.
    // Then artist and title come out trimmed.
    #[test]
    fn filename_fallback_splits_artist_title() {
        let (artist, title) = filename_fallback("Arome - Hands Up!");
        assert_eq!(artist, "Arome");
        assert_eq!(title, "Hands Up!");
    }

    // Given a stem without a separator.
    // When split by the fallback.
    // Then the stem is the title and the artist is empty.
    #[test]
    fn filename_fallback_without_separator_is_title_only() {
        let (artist, title) = filename_fallback("impulse_101");
        assert_eq!(artist, "");
        assert_eq!(title, "impulse_101");
    }

    // Given a minimal WAV file.
    // When probed.
    // Then the probe succeeds with a duration and no tags.
    #[test]
    fn probe_metadata_reads_wav_duration() {
        let bytes = wav_bytes(2.0);
        let tags = probe_metadata(&bytes, "wav").expect("probe");
        assert!(
            tags.duration_seconds.is_some(),
            "duration from codec params"
        );
        assert_eq!(tags.title, None, "no title tag in a bare WAV");
    }

    // Given garbage bytes.
    // When probed.
    // Then the probe errors rather than panicking.
    #[test]
    fn probe_metadata_rejects_garbage() {
        let result = probe_metadata(b"not audio", "wav");
        assert!(result.is_err());
    }

    /// Minimal 44.1 kHz stereo 16-bit WAV with silence.
    fn wav_bytes(seconds: f32) -> Vec<u8> {
        let rate = 44_100u32;
        #[expect(clippy::cast_possible_truncation, reason = "test fixture size")]
        #[expect(clippy::cast_sign_loss, reason = "test fixture size")]
        let frames = (f64::from(seconds) * f64::from(rate)) as usize;
        let data_len = frames * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        #[expect(clippy::cast_possible_truncation, reason = "test fixture size")]
        bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 4).to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        #[expect(clippy::cast_possible_truncation, reason = "test fixture size")]
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        bytes.resize(bytes.len() + data_len, 0);
        bytes
    }
}
