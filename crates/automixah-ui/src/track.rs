//! Track loading: pick → read → decode → hash → analyze → (override).

use std::path::{Path, PathBuf};

use error_stack::{Report, ResultExt as _};
use sha2::{Digest, Sha256};

use automixah_engine::timeline::types::TrackHash;
use djcore::analyzer::{AnalyzerOutput, BeatGrid, StratumAnalyzer};
use djcore::decoder::{DecodeAudio, DecoderRegistry};

use crate::services::Services;

/// A fully loaded track: PCM + analysis + the effective grid.
pub struct LoadedTrack {
    /// Source file path.
    pub path: PathBuf,
    /// Content hash (SHA-256 hex of file bytes) — the store key.
    pub hash: TrackHash,
    /// Decoded interleaved PCM.
    pub audio: DecodeAudio,
    /// Duration in seconds.
    pub duration_seconds: f32,
    /// Effective grid: manual override if present, else the auto grid.
    pub grid: BeatGrid,
    /// Where the effective grid came from.
    pub grid_source: GridSource,
}

/// Provenance of the effective grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSource {
    /// Manual override restored from the library.
    Manual,
    /// Auto-detected by stratum analysis.
    Auto,
}

/// Error for track loading failures.
#[derive(Debug, wherror::Error)]
#[error("track load error")]
pub struct TrackLoadError;

/// SHA-256 hex digest of the file's bytes.
///
/// Mirrors `automixah-cli`'s hashing so a future CLI remake reads the same
/// keys from the library.
fn hash_file(path: &Path) -> Result<String, Report<TrackLoadError>> {
    let bytes = std::fs::read(path)
        .change_context(TrackLoadError)
        .attach(format!("read {}", path.display()))?;
    Ok(hex(&Sha256::digest(&bytes)))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Coarse progress stage of an off-thread load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStage {
    /// Reading + hashing the file bytes.
    Hashing,
    /// Container decode to PCM.
    Decoding,
    /// Beat-grid analysis + peak extraction.
    Analyzing,
}

/// Events emitted by [`spawn_load`]; poll the receiver each frame.
pub enum LoadEvent {
    /// A stage transition (drives the status line).
    Stage(LoadStage),
    /// Terminal outcome: the loaded track plus its visual peaks, or the
    /// rendered error report (Reports are not `Send`).
    Done(Box<Result<(LoadedTrack, crate::audio::peaks::Peaks), String>>),
}

/// Spawns the load pipeline on the blocking pool.
///
/// Stages emit as they begin; the registry is constructed inside the
/// task — do not share it across threads. Override lookup happens on
/// the caller thread after `Done` (see `stored_override`).
///
/// Dropping the receiver discards the result; the task still runs to
/// completion (harmless single-shot).
pub fn spawn_load(services: &Services, path: PathBuf) -> std::sync::mpsc::Receiver<LoadEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = services.handle.clone();
    handle.spawn_blocking(move || {
        let send_stage = |stage: LoadStage| {
            let _ = tx.send(LoadEvent::Stage(stage));
        };
        let send_done = |payload: Result<(LoadedTrack, crate::audio::peaks::Peaks), String>| {
            let _ = tx.send(LoadEvent::Done(Box::new(payload)));
        };

        send_stage(LoadStage::Hashing);
        let hash = match hash_file(&path) {
            Ok(h) => TrackHash(h),
            Err(report) => return send_done(Err(format!("{report:#}"))),
        };

        send_stage(LoadStage::Decoding);
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_owned();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return send_done(Err(format!("read {}: {e}", path.display()))),
        };
        let registry = DecoderRegistry::with_symphonia();
        let audio = match registry.decode(&bytes, &extension) {
            Ok(a) => a,
            Err(report) => return send_done(Err(format!("{report:#}"))),
        };

        send_stage(LoadStage::Analyzing);
        let AnalyzerOutput {
            beat_grid: auto_grid,
            ..
        } = match analyze(&audio) {
            Ok(out) => out,
            Err(report) => return send_done(Err(format!("{report:#}"))),
        };
        #[expect(clippy::cast_precision_loss, reason = "frame count to f32")]
        let duration_seconds = audio.frames() as f32 / audio.sample_rate as f32;
        let track = LoadedTrack {
            path: path.clone(),
            hash,
            duration_seconds,
            audio,
            grid: auto_grid,
            grid_source: GridSource::Auto,
        };
        let peaks =
            crate::audio::peaks::Peaks::build(&track.audio.samples, track.audio.sample_rate);
        send_done(Ok((track, peaks)));
    });
    rx
}

/// Opens the file picker and loads the chosen track.
///
/// Returns `Ok(None)` when the dialog is cancelled.
///
/// # Errors
///
/// Returns an error if reading, decoding, or analysis fails.
pub fn open_pick(services: &Services) -> Result<Option<LoadedTrack>, Report<TrackLoadError>> {
    let registry = DecoderRegistry::with_symphonia();
    let extensions = registry.supported_extensions();
    let dialog = rfd::FileDialog::new()
        .set_title("Open audio track")
        .add_filter("audio", &extensions);
    let Some(path) = dialog.pick_file() else {
        return Ok(None);
    };
    load(&path, services, &registry).map(Some)
}

/// Loads and analyzes `path`, applying any stored manual grid override.
///
/// The async store lookup runs on the services' tokio handle via a
/// single-shot block — loading is an inherently blocking user action, so
/// this stays off the render loop.
///
/// # Errors
///
/// Returns an error if reading, decoding, or analysis fails. A store
/// lookup failure surfaces as a status note, not an error (the auto grid
/// still loads).
pub fn load(
    path: &Path,
    services: &Services,
    registry: &DecoderRegistry,
) -> Result<LoadedTrack, Report<TrackLoadError>> {
    let hash = TrackHash(hash_file(path)?);
    let bytes = std::fs::read(path)
        .change_context(TrackLoadError)
        .attach(format!("read {}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_owned();
    let audio = registry
        .decode(&bytes, &extension)
        .change_context(TrackLoadError)
        .attach("decode track")?;

    let AnalyzerOutput {
        beat_grid: auto_grid,
        ..
    } = analyze(&audio)?;

    let (grid, grid_source) = match apply_stored_override(services, &hash) {
        Ok(Some(override_grid)) => (override_grid, GridSource::Manual),
        _ => (auto_grid, GridSource::Auto),
    };

    #[expect(clippy::cast_precision_loss, reason = "frame count to f32")]
    let duration_seconds = audio.frames() as f32 / audio.sample_rate as f32;

    Ok(LoadedTrack {
        path: path.to_owned(),
        hash,
        duration_seconds,
        audio,
        grid,
        grid_source,
    })
}

/// Runs stratum analysis on the decoded audio's mono downmix.
fn analyze(audio: &DecodeAudio) -> Result<AnalyzerOutput, Report<TrackLoadError>> {
    use djcore::analyzer::AudioAnalyzer as _;
    StratumAnalyzer::new()
        .analyze(&audio.to_mono(), audio.sample_rate)
        .change_context(TrackLoadError)
        .attach("analyze track")
}

/// Rebuilds a full grid from a stored override (public: the UI thread
/// applies it after an off-thread `Done`).
pub fn apply_stored_override(
    services: &Services,
    hash: &TrackHash,
) -> Result<Option<BeatGrid>, Report<TrackLoadError>> {
    let handle = &services.handle;
    let store = services.grid_store.clone();
    let result = handle
        .block_on(async move { store.get(hash).await })
        .map_err(|report| report.change_context(TrackLoadError))?;
    Ok(result.map(|o| {
        crate::grid::EditableGrid {
            grid_bpm: o.grid_bpm,
            anchor_seconds: o.anchor_seconds,
            downbeat_phase: o.downbeat_phase,
        }
        .project()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{AppPaths, Services};
    use crate::store::{GridOverride, GridStoreService, in_memory::InMemoryGridStore};

    fn test_services_with_runtime(rt: &tokio::runtime::Runtime) -> (Services, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp");
        let services = Services {
            paths: AppPaths::for_test(dir.path()),
            grid_store: GridStoreService::new(std::sync::Arc::new(InMemoryGridStore::new())),
            handle: rt.handle().clone(),
        };
        (services, dir)
    }

    fn wav_bytes(seconds: f32) -> Vec<u8> {
        // Minimal 44.1 kHz stereo 16-bit WAV with a 440 Hz sine.
        let rate = 44_100u32;
        let frames = (f64::from(seconds) * f64::from(rate)) as usize;
        let data_len = frames * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
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
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for i in 0..frames {
            let sample = ((i as f64) * 2.0 * std::f64::consts::PI * 440.0 / f64::from(rate)).sin();
            let value = (sample * 0.5 * 32_767.0) as i16;
            bytes.extend_from_slice(&value.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    // Given a written WAV file.
    // When loaded through the full path (hash, decode, analyze).
    // Then the track decodes with the expected duration and an auto grid.
    #[test]
    fn load_decodes_and_analyzes() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let (services, dir) = test_services_with_runtime(&rt);
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");

        let registry = DecoderRegistry::with_symphonia();
        let track = load(&path, &services, &registry).expect("load");

        assert!((track.duration_seconds - 2.0).abs() < 0.05, "≈2 s");
        assert_eq!(track.grid_source, GridSource::Auto, "no override stored");
        assert!(!track.audio.samples.is_empty());
    }

    // Given a stored manual override for the file's content hash.
    // When the same file is loaded under a different name.
    // Then the manual grid wins.
    #[test]
    fn load_prefers_manual_override_by_content_hash() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let (services, dir) = test_services_with_runtime(&rt);
        let path = dir.path().join("a.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");

        let hash = TrackHash(hash_file(&path).expect("hash"));
        rt.block_on(async {
            services
                .grid_store
                .put(
                    &hash,
                    &GridOverride {
                        grid_bpm: 138.0,
                        anchor_seconds: 0.25,
                        downbeat_phase: 1,
                        updated_at: 7,
                    },
                )
                .await
                .expect("store override")
        });

        let renamed = dir.path().join("b.wav");
        std::fs::rename(&path, &renamed).expect("rename");

        let registry = DecoderRegistry::with_symphonia();
        let track = load(&renamed, &services, &registry).expect("load renamed");

        assert_eq!(track.hash, hash, "content hash survives rename");
        assert_eq!(track.grid_source, GridSource::Manual);
        assert!((track.grid.grid_bpm - 138.0).abs() < 1e-4);
    }

    // Given identical file bytes.
    // When hashed twice.
    // Then the digest is stable and hex-encoded.
    #[test]
    fn hash_file_is_stable_hex() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("x.wav");
        std::fs::write(&path, b"payload").expect("write");

        let first = hash_file(&path).expect("hash 1");
        let second = hash_file(&path).expect("hash 2");

        assert_eq!(first, second);
        assert_eq!(first.len(), 64, "SHA-256 hex");
    }

    // Given a written WAV file.
    // When loaded through the off-thread pipeline.
    // Then stages arrive in order and the track fields are complete.
    #[test]
    fn spawn_load_emits_stages_in_order() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let (services, dir) = test_services_with_runtime(&rt);
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");

        let rx = spawn_load(&services, path.clone());
        let mut stages = Vec::new();
        let outcome = loop {
            match rx.recv() {
                Ok(LoadEvent::Stage(s)) => stages.push(s),
                Ok(LoadEvent::Done(payload)) => break *payload,
                Err(_) => unreachable!("channel closed before Done"),
            }
        };

        assert_eq!(
            stages,
            [
                LoadStage::Hashing,
                LoadStage::Decoding,
                LoadStage::Analyzing
            ],
            "stage order"
        );
        let (track, peaks) = outcome.expect("load ok");
        assert!((track.duration_seconds - 2.0).abs() < 0.05, "≈2 s");
        assert_eq!(track.path, path);
        assert_eq!(track.grid_source, GridSource::Auto);
        assert!(!peaks.data.is_empty(), "peaks built off-thread");
    }

    // Given a nonexistent path.
    // When spawned.
    // Then Done carries the rendered error, no panic.
    #[test]
    fn spawn_load_reports_missing_file() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let (services, dir) = test_services_with_runtime(&rt);
        let rx = spawn_load(&services, dir.path().join("nope.wav"));
        let outcome = loop {
            match rx.recv() {
                Ok(LoadEvent::Done(payload)) => break *payload,
                Ok(_) => {}
                Err(_) => unreachable!("channel closed before Done"),
            }
        };
        let err = match outcome {
            Err(e) => e,
            Ok(_) => unreachable!("missing file must fail"),
        };
        assert!(
            err.contains("track load error"),
            "rendered report present: {err}"
        );
    }
}
