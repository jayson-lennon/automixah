//! Track loading: pick → read → decode → hash → analyze → (override).
//!
//! One pipeline, bus-only reporting: the task sends `LoadStage` events
//! as it progresses and terminates with `LoadDone` (plus the
//! hash-addressed analysis events the playlist derives from). Analysis
//! runs through the injected `services.analyzer` — no production path
//! constructs an analyzer directly.
//!
use std::path::PathBuf;

use automixah_engine::timeline::types::{CuePoints, TrackHash};
use djcore::analyzer::AnalyzerOutput;
use djcore::decoder::{DecodeAudio, DecoderRegistry};

use crate::bus::{Event, LoadOutcome};
use crate::services::Services;
use crate::tracks::Analysis;

/// Track identity and tag resolution: the single home for the helpers
/// every track pipeline shares (hash, tags, extension, clock).
pub mod identity {
    use std::path::Path;

    /// SHA-256 hex digest of bytes — the content hash every subsystem
    /// addresses tracks by.
    #[must_use]
    pub fn hex_sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let mut out = String::with_capacity(digest.len() * 2);
        for b in digest {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// Resolved display tags: container tags when present, filename
    /// fallback otherwise.
    #[must_use]
    pub fn resolve_tags(bytes: &[u8], path: &Path) -> crate::tracks::TrackTags {
        let extension = extension_of(path);
        let probed = djcore::decoder::meta::probe_metadata(bytes, &extension).ok();
        let (fallback_artist, fallback_title) = path.file_stem().and_then(|s| s.to_str()).map_or(
            (String::new(), String::new()),
            djcore::decoder::meta::filename_fallback,
        );
        crate::tracks::TrackTags {
            title: probed
                .as_ref()
                .and_then(|t| t.title.clone())
                .unwrap_or(fallback_title),
            artist: probed
                .as_ref()
                .and_then(|t| t.artist.clone())
                .unwrap_or(fallback_artist),
            path: path.to_owned(),
        }
    }

    /// Container-probed duration in seconds, when known.
    #[must_use]
    pub fn probe_duration(bytes: &[u8], path: &Path) -> Option<f64> {
        let extension = extension_of(path);
        djcore::decoder::meta::probe_metadata(bytes, &extension)
            .ok()
            .and_then(|t| t.duration_seconds)
            .map(f64::from)
    }

    /// Lowercase extension of a path (empty when absent).
    #[must_use]
    pub fn extension_of(path: &Path) -> String {
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase()
    }

    /// Current unix time in seconds (0 on clock failure).
    #[must_use]
    pub fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }
}

/// Error for track loading failures.
#[derive(Debug, wherror::Error)]
#[error(debug)]
pub struct TrackLoadError;

/// Coarse progress stage of an off-thread load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStage {
    /// Reading + hashing the file bytes.
    Hashing,
    /// Container decode to PCM.
    Decoding,
    /// Beat-grid analysis + peak extraction.
    Analyzing,
    /// Analysis skipped: a stored grid covers this exact content.
    CacheHit,
}

/// Spawns the load pipeline on the blocking pool, reporting through the
/// bus like every other async task (no mpsc channel to poll).
///
/// Stages emit as they begin; the registry is constructed inside the
/// task — do not share it across threads. The terminal `LoadDone`
/// carries the full outcome (analysis package, PCM, peaks) the deck is
/// built from; `AnalysisDone`/`AnalysisFailed` also land so playlist
/// rows referencing the hash derive correctly.
pub fn spawn_load(services: &Services, tx: std::sync::mpsc::Sender<Event>, path: PathBuf) {
    let services = services.clone();
    let handle = services.runtime.handle().clone();
    let block_handle = services.runtime.handle().clone();
    handle.spawn_blocking(move || {
        let send = |event: Event| {
            let _ = tx.send(event);
        };
        let send_stage = |stage: LoadStage| {
            send(Event::LoadStage(stage));
        };
        let fail = |hash: &TrackHash, message: String| {
            send(Event::AnalysisFailed {
                hash: hash.clone(),
                message: message.clone(),
            });
            send(Event::LoadDone(Box::new(Err(message))));
        };

        send_stage(LoadStage::Hashing);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                let message = format!("read {}: {e}", path.display());
                // No hash yet — the failure only reaches the editor.
                send(Event::LoadDone(Box::new(Err(message))));
                return;
            }
        };
        let hash = TrackHash(identity::hex_sha256(&bytes));

        send_stage(LoadStage::Decoding);
        let extension = identity::extension_of(&path);
        let registry = DecoderRegistry::with_symphonia();
        let audio = match registry.decode(&bytes, &extension) {
            Ok(a) => a,
            Err(report) => return fail(&hash, format!("{report:#}")),
        };

        // Analysis reuse: a stored grid (manual override or the persisted
        // auto grid) means the content was already analyzed — skip
        // straight to it.
        let stored: Option<crate::grid::EditableGrid> = {
            let store = services.grid_store.clone();
            let lookup_hash = hash.clone();
            block_handle
                .block_on(async { store.get(&lookup_hash).await })
                .ok()
                .flatten()
                .map(|o| crate::grid::EditableGrid {
                    grid_bpm: o.grid_bpm,
                    anchor_seconds: o.anchor_seconds,
                    downbeat_phase: o.downbeat_phase,
                })
        };

        let analysis = match stored {
            Some(grid) => {
                send_stage(LoadStage::CacheHit);
                let store = services.grid_store.clone();
                let lookup_hash = hash.clone();
                let stored_full = block_handle
                    .block_on(async { store.get(&lookup_hash).await })
                    .ok()
                    .flatten();
                let key = stored_full.and_then(|o| o.key).unwrap_or(djcore::key::Key {
                    root: 0,
                    mode: djcore::key::KeyMode::Major,
                });
                let beats_grid = grid.project();
                let bpm = beats_grid.grid_bpm;
                #[expect(clippy::cast_precision_loss, reason = "frame count to f32")]
                let duration = audio.frames() as f32 / audio.sample_rate as f32;
                let cues = block_handle
                    .block_on(async { services.cue_store.get(&lookup_hash).await })
                    .ok()
                    .unwrap_or_default();
                Analysis {
                    grid: beats_grid,
                    bpm,
                    key,
                    duration_seconds: duration,
                    cues,
                }
            }
            None => {
                send_stage(LoadStage::Analyzing);
                send(Event::AnalysisStarted { hash: hash.clone() });
                let output = match services
                    .analyzer
                    .analyze(&audio.to_mono(), audio.sample_rate)
                {
                    Ok(out) => out,
                    Err(report) => return fail(&hash, format!("{report:#}")),
                };
                persist_fresh(&services, &block_handle, &hash, &output);
                let cues = {
                    let store = services.cue_store.clone();
                    let lookup_hash = hash.clone();
                    block_handle
                        .block_on(async { store.get(&lookup_hash).await })
                        .ok()
                        .unwrap_or_default()
                };
                analysis_from(&output, &audio, cues)
            }
        };

        let peaks = crate::audio::peaks::Peaks::build_with_channels(
            &audio.samples,
            audio.sample_rate,
            audio.channels,
        );
        send(Event::AnalysisDone {
            hash: hash.clone(),
            analysis: analysis.clone(),
        });
        send(Event::LoadDone(Box::new(Ok(LoadOutcome {
            hash,
            path,
            analysis,
            audio,
            peaks,
        }))));
    });
}

/// An instant-preview request. Identity is supplied, not derived: rows
/// and entries already carry content hashes, so the preview path reads,
/// decodes, and nothing else.
#[derive(Debug, Clone)]
pub struct PreviewJob {
    /// Content hash the outcome is addressed by (trusted, never recomputed).
    pub hash: TrackHash,
    /// File to read + decode.
    pub path: PathBuf,
}

/// Spawns the instant-preview pipeline on the blocking pool: read →
/// decode, full stop. No analysis, no store lookups, no peaks, no
/// persistence, no stage events — the faster the decode lands, the sooner
/// sound starts. Exactly one terminal event reaches the bus
/// ([`Event::PreviewLoaded`] or [`Event::PreviewFailed`]).
pub fn spawn_preview_load(
    services: &Services,
    tx: std::sync::mpsc::Sender<Event>,
    job: PreviewJob,
) {
    let handle = services.runtime.handle().clone();
    handle.spawn_blocking(move || {
        let send = |event: Event| {
            let _ = tx.send(event);
        };
        let fail = |message: String| {
            send(Event::PreviewFailed {
                hash: job.hash.clone(),
                message,
            })
        };

        let bytes = match std::fs::read(&job.path) {
            Ok(b) => b,
            Err(e) => return fail(format!("read {}: {e}", job.path.display())),
        };
        let extension = identity::extension_of(&job.path);
        let registry = DecoderRegistry::with_symphonia();
        match registry.decode(&bytes, &extension) {
            Ok(audio) => send(Event::PreviewLoaded {
                hash: job.hash,
                audio,
            }),
            Err(report) => fail(format!("{report:#}")),
        }
    });
}

/// Builds the analysis package from an analyzer output.
fn analysis_from(output: &AnalyzerOutput, audio: &DecodeAudio, cues: CuePoints) -> Analysis {
    #[expect(clippy::cast_precision_loss, reason = "frame count to f32")]
    let duration = audio.frames() as f32 / audio.sample_rate as f32;
    Analysis {
        grid: output.beat_grid.clone(),
        bpm: output.bpm,
        key: output.key.clone(),
        duration_seconds: duration,
        cues,
    }
}

/// Persists a freshly detected grid + key for `hash`.
fn persist_fresh(
    services: &Services,
    block_handle: &tokio::runtime::Handle,
    hash: &TrackHash,
    output: &AnalyzerOutput,
) {
    let store = services.grid_store.clone();
    if let Err(report) = block_handle.block_on(async {
        store
            .put(
                hash,
                &crate::store::GridOverride {
                    grid_bpm: output.beat_grid.grid_bpm,
                    anchor_seconds: output.beat_grid.anchor_seconds,
                    downbeat_phase: crate::grid::EditableGrid::from_grid(&output.beat_grid)
                        .downbeat_phase,
                    updated_at: identity::now_unix(),
                    key: Some(output.key.clone()),
                },
            )
            .await
    }) {
        // Persistence failure must not fail the load: the deck still
        // builds from the fresh analysis; the next save retries.
        eprintln!("persist fresh grid: {report:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::services::{AppPaths, Services};
    use crate::store::in_memory::InMemoryGridStore;
    use crate::store::{GridOverride, GridStoreService};

    fn test_services() -> (Services, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp");
        let runtime = std::sync::Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        let services = Services {
            paths: AppPaths::for_test(dir.path()),
            grid_store: GridStoreService::new(std::sync::Arc::new(InMemoryGridStore::new())),
            cue_store: crate::store::CueStoreService::new(std::sync::Arc::new(
                crate::store::in_memory::InMemoryCueStore::new(),
            )),
            playlist_store: crate::playlist::store::PlaylistStoreService::new(std::sync::Arc::new(
                crate::playlist::store::in_memory::InMemoryPlaylistStore::new(),
            )),
            library_store: crate::library::store::LibraryStoreService::new(std::sync::Arc::new(
                crate::library::store::in_memory::InMemoryLibraryStore::new(),
            )),
            scan_latch: std::sync::Arc::default(),
            analyzer: std::sync::Arc::new(djcore::analyzer::FakeAnalyzer::with_output(
                djcore::analyzer::AnalyzerOutput {
                    bpm: 128.0,
                    key: djcore::key::Key {
                        root: 9,
                        mode: djcore::key::KeyMode::Minor,
                    },
                    duration_seconds: 2.0,
                    beat_grid: djcore::analyzer::BeatGrid {
                        grid_bpm: 128.0,
                        ..Default::default()
                    },
                    bpm_confidence: 1.0,
                    key_confidence: 1.0,
                    grid_stability: 1.0,
                },
            )),
            runtime,
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

    /// Drains bus events until `LoadDone` lands.
    fn drain_done(bus: &EventBus) -> (Vec<Event>, Result<LoadOutcome, String>) {
        let rx = bus.receiver_for_test();
        let mut events = Vec::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(Event::LoadDone(boxed)) => {
                    let outcome = *boxed;
                    return (events, outcome);
                }
                Ok(event) => events.push(event),
                Err(_) => panic!("bus closed before LoadDone"),
            }
        }
    }

    // Given a written WAV file.
    // When loaded in normal mode.
    // Then stages arrive in order and the outcome fields are complete.
    #[test]
    fn spawn_load_emits_stages_in_order() {
        let (services, dir) = test_services();
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");

        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), path.clone());

        let (events, outcome) = drain_done(&bus);
        let stages: Vec<LoadStage> = events
            .iter()
            .filter_map(|e| match e {
                Event::LoadStage(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(
            stages,
            [
                LoadStage::Hashing,
                LoadStage::Decoding,
                LoadStage::Analyzing
            ],
            "stage order"
        );
        let ok = outcome.expect("load ok");
        assert!((ok.analysis.duration_seconds - 2.0).abs() < 0.05, "≈2 s");
        assert_eq!(ok.path, path);
        assert!(!ok.peaks.data.is_empty(), "peaks built off-thread");
    }

    // Given a first load persisted its auto grid to the store.
    // When the same file loads again in normal mode.
    // Then the store row short-circuits analysis (cache-hit stage).
    #[test]
    fn normal_load_reuses_persisted_grid() {
        let (services, dir) = test_services();
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");

        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), path.clone());
        let (first, _) = drain_done(&bus);
        assert!(
            first
                .iter()
                .any(|e| matches!(e, Event::LoadStage(LoadStage::Analyzing))),
            "first pass analyzes"
        );

        spawn_load(&services, bus.sender(), path);
        let (second, outcome) = drain_done(&bus);
        assert!(
            second
                .iter()
                .any(|e| matches!(e, Event::LoadStage(LoadStage::CacheHit))),
            "second pass must reuse the persisted grid"
        );
        assert!(outcome.is_ok());
    }

    // Given a stored manual override for the file's content hash.
    // When the same content loads under a different name in normal mode.
    // Then the override grid wins by content hash.
    #[test]
    fn normal_load_prefers_manual_override_by_content_hash() {
        let (services, dir) = test_services();
        let path = dir.path().join("a.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");

        let hash = TrackHash(identity::hex_sha256(&bytes));
        services.runtime.block_on(async {
            services
                .grid_store
                .put(
                    &hash,
                    &GridOverride {
                        grid_bpm: 138.0,
                        anchor_seconds: 0.25,
                        downbeat_phase: 1,
                        updated_at: 7,
                        key: None,
                    },
                )
                .await
                .expect("store override");
        });

        let renamed = dir.path().join("b.wav");
        std::fs::rename(&path, &renamed).expect("rename");

        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), renamed);

        let (_, outcome) = drain_done(&bus);
        let ok = outcome.expect("load renamed");
        assert_eq!(ok.hash, hash, "content hash survives rename");
        assert!(
            (ok.analysis.grid.grid_bpm - 138.0).abs() < 1e-4,
            "override grid wins"
        );
    }

    // Given a cache-hit grid and independently persisted cue points.
    // When the editor loads the file.
    // Then the cache-hit outcome carries the cue snapshot.
    #[test]
    fn cache_hit_load_hydrates_persisted_cues() {
        let (services, dir) = test_services();
        let path = dir.path().join("cached.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");
        let hash = TrackHash(identity::hex_sha256(&bytes));
        let cues = CuePoints::with_in(3, 44_100 * 12);

        services.runtime.block_on(async {
            services
                .grid_store
                .put(
                    &hash,
                    &GridOverride {
                        grid_bpm: 128.0,
                        anchor_seconds: 0.0,
                        downbeat_phase: 0,
                        updated_at: 1,
                        key: None,
                    },
                )
                .await
                .expect("seed grid");
            services
                .cue_store
                .put(&hash, &cues)
                .await
                .expect("seed cues");
        });

        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), path);
        let (events, outcome) = drain_done(&bus);

        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::LoadStage(LoadStage::CacheHit))),
            "stored grid takes the cache-hit path"
        );
        assert_eq!(outcome.expect("load").analysis.cues, cues);
    }

    // Given independently persisted cue points but no stored grid.
    // When the editor performs fresh analysis.
    // Then the fresh outcome retains the existing cue snapshot.
    #[test]
    fn fresh_load_hydrates_persisted_cues() {
        let (services, dir) = test_services();
        let path = dir.path().join("fresh.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");
        let hash = TrackHash(identity::hex_sha256(&bytes));
        let cues = CuePoints {
            outs: [None, Some(44_100 * 20), None, None],
            ..CuePoints::default()
        };
        services.runtime.block_on(async {
            services
                .cue_store
                .put(&hash, &cues)
                .await
                .expect("seed cues");
        });

        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), path);
        let (events, outcome) = drain_done(&bus);

        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::LoadStage(LoadStage::Analyzing))),
            "missing grid takes the fresh-analysis path"
        );
        assert_eq!(outcome.expect("load").analysis.cues, cues);
    }

    // Given a nonexistent path.
    // When spawned.
    // Then LoadDone carries the rendered error, no panic.
    #[test]
    fn spawn_load_reports_missing_file() {
        let (services, dir) = test_services();
        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), dir.path().join("nope.wav"));

        let err = match drain_done(&bus).1 {
            Err(e) => e,
            Ok(_) => panic!("missing file must fail"),
        };
        assert!(err.contains("nope.wav"), "rendered error present: {err}");
    }

    // Given a written WAV file.
    // When preview-spawned.
    // Then exactly one terminal event carries the decoded PCM — and no
    // analysis-related event ever fires (the whole point of the path).
    #[test]
    fn spawn_preview_emits_pcm_without_analysis() {
        let (services, dir) = test_services();
        let analyzer = std::sync::Arc::new(djcore::analyzer::FakeAnalyzer::with_output(
            djcore::analyzer::AnalyzerOutput {
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 2.0,
                beat_grid: Default::default(),
                bpm_confidence: 1.0,
                key_confidence: 1.0,
                grid_stability: 1.0,
            },
        ));
        let services = Services {
            analyzer: analyzer.clone(),
            ..services
        };
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");
        let bytes = std::fs::read(&path).expect("read back");
        let hash = TrackHash(identity::hex_sha256(&bytes));

        let bus = EventBus::without_repaint();
        spawn_preview_load(
            &services,
            bus.sender(),
            PreviewJob {
                hash: hash.clone(),
                path,
            },
        );

        let rx = bus.receiver_for_test();
        let mut events = Vec::new();
        // Drain until the terminal preview event lands (the bus holds its
        // own sender, so the channel never disconnects).
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(event @ (Event::PreviewLoaded { .. } | Event::PreviewFailed { .. })) => {
                    events.push(event);
                    break;
                }
                Ok(event) => events.push(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("bus hung"),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("bus closed"),
            }
        }
        let Event::PreviewLoaded {
            hash: loaded,
            audio,
        } = &events[events.len() - 1]
        else {
            let names: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
            panic!("expected PreviewLoaded terminal, got [{names:?}]");
        };
        assert_eq!(loaded, &hash);
        assert_eq!(audio.sample_rate, 44_100);
        assert_eq!(audio.frames(), 2 * 44_100_usize, "two seconds of frames");
        assert!(!audio.samples.is_empty());
        // And nothing analytical happened anywhere on the path.
        assert_eq!(analyzer.call_count(), 0, "preview must not analyze");
        assert!(
            !events.iter().any(|e| matches!(
                e,
                Event::AnalysisStarted { .. } | Event::AnalysisDone { .. }
            )),
            "no analysis events may fire"
        );
    }

    // Given identical content stored under two different filenames.
    // When preview-spawned.
    // Then the caller-supplied identity is trusted — no hashing work runs.
    #[test]
    fn spawn_preview_trusts_supplied_hash_not_content() {
        let (services, dir) = test_services();
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(1.0)).expect("write wav");

        let bus = EventBus::without_repaint();
        let supplied = TrackHash("not-the-real-hash".to_owned());
        spawn_preview_load(
            &services,
            bus.sender(),
            PreviewJob {
                hash: supplied.clone(),
                path,
            },
        );

        let rx = bus.receiver_for_test();
        let event = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("one terminal event");
        match event {
            Event::PreviewLoaded { hash, audio } => {
                assert_eq!(hash, supplied, "identity passes through untouched");
                assert!(!audio.samples.is_empty());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // Given a nonexistent file.
    // When preview-spawned.
    // Then PreviewFailed is the terminal event and it is addressed by hash.
    #[test]
    fn spawn_preview_reports_missing_file_by_hash() {
        let (services, dir) = test_services();

        let bus = EventBus::without_repaint();
        let hash = TrackHash("deadbeef".to_owned());
        spawn_preview_load(
            &services,
            bus.sender(),
            PreviewJob {
                hash: hash.clone(),
                path: dir.path().join("nope.wav"),
            },
        );

        let rx = bus.receiver_for_test();
        let event = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("terminal event");
        match event {
            Event::PreviewFailed {
                hash: failed_hash,
                message,
            } => {
                assert_eq!(failed_hash, hash);
                assert!(message.contains("nope.wav"), "rendered error: {message}");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // Given identical file bytes.
    // When hashed twice.
    // Then the digest is stable and hex-encoded.
    #[test]
    fn hash_file_is_stable_hex() {
        let payload = b"payload";
        let first = identity::hex_sha256(payload);
        let second = identity::hex_sha256(payload);

        assert_eq!(first, second);
        assert_eq!(first.len(), 64, "SHA-256 hex");
    }

    // Given a FakeAnalyzer injected in services.
    // When a load runs.
    // Then the injected analyzer is called (injection honored).
    #[test]
    fn load_uses_injected_analyzer() {
        let (services, dir) = test_services();
        let analyzer = std::sync::Arc::new(djcore::analyzer::FakeAnalyzer::with_output(
            djcore::analyzer::AnalyzerOutput {
                bpm: 100.0,
                key: djcore::key::Key {
                    root: 3,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 2.0,
                beat_grid: Default::default(),
                bpm_confidence: 1.0,
                key_confidence: 1.0,
                grid_stability: 1.0,
            },
        ));
        let services = Services {
            analyzer: analyzer.clone(),
            ..services
        };
        let path = dir.path().join("tone.wav");
        std::fs::write(&path, wav_bytes(2.0)).expect("write wav");

        let bus = EventBus::without_repaint();
        spawn_load(&services, bus.sender(), path);
        let _ = drain_done(&bus);

        assert_eq!(analyzer.call_count(), 1, "injected analyzer called");
    }
}
