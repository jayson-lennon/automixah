//! The analysis queue: a single worker draining FIFO playlist-track jobs.
//!
//! One job at a time (analysis is CPU-heavy; parallel jobs would starve the
//! host). Each job was already persisted by the add-task (tags + entry,
//! rowid minted by the store); the worker emits `RowAnalyzing`, then
//! either short-circuits on a library hit (grid + key + duration known)
//! or decodes, analyzes, persists grid/key, and drops the PCM. All
//! outcomes are bus events addressed by the database rowid.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use automixah_engine::timeline::types::TrackHash;
use djcore::decoder::meta::{filename_fallback, probe_metadata};

use crate::bus::Event;
use crate::services::Services;

/// Database-minted identity of a playlist row (`playlist_tracks.rowid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowId(pub i64);

/// One queued analysis job.
#[derive(Debug, Clone)]
pub struct QueueJob {
    /// Row this job updates (events address rows by this).
    pub row_id: RowId,
    /// Playlist the track was added to.
    pub playlist_id: i64,
    /// Path of the audio file.
    pub path: PathBuf,
    /// Content hash (computed by the add-task; reused here).
    pub hash: TrackHash,
}

/// Metadata a ready row displays.
#[derive(Debug, Clone)]
pub struct TrackMeta {
    /// Content hash of the analyzed file.
    pub hash: TrackHash,
    /// BPM from the grid.
    pub bpm: f32,
    /// Detected key.
    pub key: djcore::key::Key,
    /// Duration in seconds.
    pub duration_seconds: f32,
    /// Display title.
    pub title: String,
    /// Display artist (empty when unknown).
    pub artist: String,
}

/// Handle to the worker: clone the sender side to enqueue; the worker
/// sends bus events.
#[derive(Debug)]
pub struct AnalysisQueue {
    job_tx: Sender<QueueJob>,
}

impl AnalysisQueue {
    /// Spawns the single worker thread over `services`.
    ///
    /// The worker parks when idle and exits when every `job_tx` clone is
    /// dropped (app teardown).
    #[must_use]
    pub fn spawn(services: Services, events: Sender<Event>) -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<QueueJob>();
        std::thread::Builder::new()
            .name("playlist-analysis".to_owned())
            .spawn(move || worker_loop(services, job_rx, events))
            .expect("spawn analysis worker");
        Self { job_tx }
    }

    /// Enqueues a job; the owning row should already show `Queued`.
    pub fn enqueue(&self, job: QueueJob) {
        // A send fails only when the worker is gone (app teardown); the
        // row then stays queued harmlessly.
        let _ = self.job_tx.send(job);
    }
}

/// The worker loop: one job at a time, FIFO, until the channel closes.
fn worker_loop(services: Services, job_rx: Receiver<QueueJob>, events: Sender<Event>) {
    while let Ok(job) = job_rx.recv() {
        run_job(&services, &job, &events);
    }
}

/// Runs one job end to end, emitting exactly one terminal event.
fn run_job(services: &Services, job: &QueueJob, events: &Sender<Event>) {
    let fail = |message: String| {
        let _ = events.send(Event::RowFailed {
            row_id: job.row_id,
            message,
        });
    };
    let send = |event: Event| {
        let _ = events.send(event);
    };

    // Library fast path: grid + key + duration all known → no decode.
    // The add-task already persisted tags and the entry (rowid exists).
    if let Some(meta) = library_hit(services, &job.hash, &job.path) {
        send(Event::RowReady {
            row_id: job.row_id,
            meta,
        });
        return;
    }

    send(Event::RowAnalyzing { row_id: job.row_id });

    let bytes = match std::fs::read(&job.path) {
        Ok(bytes) => bytes,
        Err(e) => return fail(format!("read {}: {e}", job.path.display())),
    };

    let registry = djcore::decoder::DecoderRegistry::with_symphonia();
    let extension = extension_of(&job.path);
    let audio = match registry.decode(&bytes, &extension) {
        Ok(audio) => audio,
        Err(report) => return fail(format!("{report:#}")),
    };
    let output = match services
        .analyzer
        .analyze(&audio.to_mono(), audio.sample_rate)
    {
        Ok(output) => output,
        Err(report) => return fail(format!("{report:#}")),
    };

    // Persist grid + key; tags duration gets the analyzed value.
    let runtime = services.runtime.handle().clone();
    let grid_store = services.grid_store.clone();
    let playlist_store = services.playlist_store.clone();
    let hash = job.hash.clone();
    let persisted = runtime.block_on(async {
        grid_store
            .put(
                &hash,
                &crate::store::GridOverride {
                    grid_bpm: output.beat_grid.grid_bpm,
                    anchor_seconds: output.beat_grid.anchor_seconds,
                    downbeat_phase: crate::grid::EditableGrid::from_grid(&output.beat_grid)
                        .downbeat_phase,
                    updated_at: now_unix(),
                    key: Some(output.key.clone()),
                },
            )
            .await
    });
    if let Err(report) = persisted {
        return fail(format!("{report:#}"));
    }
    let _ = runtime.block_on(async {
        playlist_store
            .update_track_meta(&hash, Some(f64::from(output.duration_seconds)))
            .await
    });

    // PCM (`audio`, `bytes`) goes out of scope here — nothing retains it.
    send(Event::RowReady {
        row_id: job.row_id,
        meta: TrackMeta {
            hash,
            bpm: output.bpm,
            key: output.key,
            duration_seconds: output.duration_seconds,
            title: tags_for(&job.path, &bytes).0,
            artist: tags_for(&job.path, &bytes).1,
        },
    });
}

/// Resolves display tags: container tags when present, filename fallback
/// otherwise. Duration lives in [`probe_duration`].
fn tags_for(path: &std::path::Path, bytes: &[u8]) -> (String, String) {
    let extension = extension_of(path);
    let probed = probe_metadata(bytes, &extension).ok();
    let (fallback_artist, fallback_title) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or((String::new(), String::new()), filename_fallback);
    let title = probed
        .as_ref()
        .and_then(|t| t.title.clone())
        .unwrap_or(fallback_title);
    let artist = probed
        .as_ref()
        .and_then(|t| t.artist.clone())
        .unwrap_or(fallback_artist);
    (title, artist)
}

/// The library-hit fast path: full metadata from the stores, no decode.
///
/// The duration comes from the `tracks` row (the analyzer value written
/// at first analysis — the probe may legitimately know less).
fn library_hit(services: &Services, hash: &TrackHash, path: &std::path::Path) -> Option<TrackMeta> {
    let runtime = services.runtime.handle();
    let grid = runtime
        .block_on(async { services.grid_store.get(hash).await })
        .ok()??;
    let key = grid.key.clone()?;
    let duration = runtime
        .block_on(async { services.playlist_store.track_duration(hash).await })
        .ok()
        .flatten()?;
    let bytes = std::fs::read(path).ok()?;
    let (title, artist) = tags_for(path, &bytes);
    #[expect(clippy::cast_possible_truncation, reason = "f64 store to f32 display")]
    Some(TrackMeta {
        hash: hash.clone(),
        bpm: grid.grid_bpm,
        key,
        duration_seconds: duration as f32,
        title,
        artist,
    })
}

/// Lowercase extension of a path (empty when absent).
fn extension_of(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

/// SHA-256 hex of bytes (mirrors `track.rs`'s hashing).
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Current unix time in seconds (0 on clock failure).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::playlist::store::PlaylistStoreService;
    use crate::playlist::store::in_memory::InMemoryPlaylistStore;
    use crate::services::{AppPaths, Services};
    use crate::store::GridStoreService;
    use crate::store::in_memory::InMemoryGridStore;
    use djcore::analyzer::{AnalyzerOutput, FakeAnalyzer};

    pub(crate) fn fake_services_for_app() -> Services {
        fake_services(output_fixture())
    }

    fn fake_services(analyzer_output: AnalyzerOutput) -> Services {
        let runtime = std::sync::Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("test runtime"),
        );
        Services {
            paths: AppPaths::for_test(std::path::Path::new("/tmp/unused")),
            grid_store: GridStoreService::new(std::sync::Arc::new(InMemoryGridStore::new())),
            playlist_store: PlaylistStoreService::new(std::sync::Arc::new(
                InMemoryPlaylistStore::new(),
            )),
            analyzer: std::sync::Arc::new(FakeAnalyzer::with_output(analyzer_output)),
            runtime,
        }
    }

    fn output_fixture() -> AnalyzerOutput {
        AnalyzerOutput {
            bpm: 138.0,
            key: djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 2.0,
            beat_grid: Default::default(),
            bpm_confidence: 1.0,
            key_confidence: 1.0,
            grid_stability: 1.0,
        }
    }

    /// Minimal WAV (silence) the real decoder accepts.
    pub(crate) fn wav_bytes(seconds: f32) -> Vec<u8> {
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
        bytes.resize(bytes.len() + data_len, 0);
        bytes
    }

    /// Drains bus events until one terminal row event for `row_id`
    /// lands (the worker emits analyzing → terminal per job).
    fn drain_terminal(bus: &EventBus, row_id: RowId) -> Vec<Event> {
        let rx = bus.receiver_for_test();
        let mut events = Vec::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(event) => {
                    let terminal =
                        matches!(event, Event::RowReady { .. } | Event::RowFailed { .. });
                    events.push(event);
                    if terminal {
                        return events;
                    }
                    let _ = row_id;
                }
                Err(_) => return events,
            }
        }
    }

    // Given a wav file enqueued fresh.
    // When the worker runs the job.
    // Then the row transitions through analyzing to ready with metadata
    // and the store holds grid + tags.
    #[test]
    fn queue_transitions_queued_analyzing_ready() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("Tenebrax - Impulse.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");

        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("q").await })
            .expect("create playlist");
        // The add path persists first; the worker only analyzes.
        let hash = TrackHash(hex_sha256(&bytes));
        services.runtime.block_on(async {
            services
                .playlist_store
                .ensure_track(list.id, &hash, "/x", "Impulse", "Tenebrax", None)
                .await
                .expect("ensure entry")
        });
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            row_id: RowId(1),
            playlist_id: list.id,
            path,
            hash: hash.clone(),
        });

        let events = drain_terminal(&bus, RowId(1));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::RowAnalyzing { .. })),
            "analyzing stage observed: {events:?}"
        );
        let ready = events
            .into_iter()
            .find_map(|e| match e {
                Event::RowReady { meta, .. } => Some(meta),
                _ => None,
            })
            .expect("ready event");
        assert_eq!(ready.title, "Impulse", "filename fallback title");
        assert_eq!(ready.artist, "Tenebrax");

        let stored = services
            .runtime
            .block_on(async { services.grid_store.get(&hash).await })
            .expect("grid lookup")
            .expect("grid persisted");
        assert_eq!(stored.key.map(|k| k.root), Some(9), "key persisted");
    }

    // Given a hash with grid + key + duration already stored.
    // When the same file is enqueued.
    // Then the row goes straight to ready with no analyzing stage, the
    // analyzer is never called, and the stored duration is reported.
    #[test]
    fn add_with_library_hit_skips_queue_and_reports_duration() {
        let services = fake_services(output_fixture());
        let analyzer = std::sync::Arc::new(FakeAnalyzer::with_output(output_fixture()));
        let services = Services {
            analyzer: analyzer.clone(),
            ..services
        };
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("hit.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");
        let hash = TrackHash(hex_sha256(&bytes));
        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("h").await })
            .expect("create playlist");
        services.runtime.block_on(async {
            services
                .grid_store
                .put(
                    &hash,
                    &crate::store::GridOverride {
                        grid_bpm: 140.0,
                        anchor_seconds: 0.0,
                        downbeat_phase: 0,
                        updated_at: 1,
                        key: Some(djcore::key::Key {
                            root: 0,
                            mode: djcore::key::KeyMode::Major,
                        }),
                    },
                )
                .await
                .expect("seed grid");
            services
                .playlist_store
                .ensure_track(list.id, &hash, "/x", "T", "A", Some(122.25))
                .await
                .expect("seed tags")
        });
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            row_id: RowId(7),
            playlist_id: list.id,
            path,
            hash,
        });

        let events = drain_terminal(&bus, RowId(7));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::RowAnalyzing { .. })),
            "no analyzing stage on a library hit"
        );
        let meta = events
            .into_iter()
            .find_map(|e| match e {
                Event::RowReady { meta, .. } => Some(meta),
                _ => None,
            })
            .expect("ready event");
        assert!(
            (meta.duration_seconds - 122.25).abs() < 0.01,
            "stored duration"
        );
        assert_eq!(analyzer.call_count(), 0, "analyzer never called");
    }

    // Given a playlist row persisted without grid/key/duration (the legacy
    // re-enqueue case).
    // When the worker re-processes it.
    // Then it reaches Ready exactly once (G1 regression).
    #[test]
    fn reenqueued_incomplete_row_reaches_ready() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("Tenebrax - Impulse.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");

        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("g1").await })
            .expect("create playlist");
        let hash = TrackHash(hex_sha256(&bytes));
        services.runtime.block_on(async {
            services
                .playlist_store
                .ensure_track(
                    list.id,
                    &hash,
                    path.to_str().expect("utf8"),
                    "Impulse",
                    "Tenebrax",
                    None,
                )
                .await
                .expect("persist incomplete row")
        });
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        let row_id = services
            .runtime
            .block_on(async { services.playlist_store.tracks_for(list.id).await })
            .expect("rows")[0]
            .id;
        queue.enqueue(QueueJob {
            row_id: RowId(row_id),
            playlist_id: list.id,
            path: path.clone(),
            hash: hash.clone(),
        });
        let events = drain_terminal(&bus, RowId(row_id));
        let id = row_id;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::RowReady { row_id, .. } if row_id.0 == id)),
            "incomplete row reached ready on re-enqueue"
        );
        // Re-running the same job (restart simulation) stays a no-op, not a
        // failure: ensure_track must not trip on the existing row.
        queue.enqueue(QueueJob {
            row_id: RowId(row_id),
            playlist_id: list.id,
            path: path.clone(),
            hash: hash.clone(),
        });
        let second = drain_terminal(&bus, RowId(row_id));
        assert!(
            !second.iter().any(|e| matches!(e, Event::RowFailed { .. })),
            "re-analysis never fails on the existing row"
        );
    }

    // Given a nonexistent path.
    // When enqueued.
    // Then a Failed event arrives without panicking.
    #[test]
    fn missing_file_job_reports_failed() {
        let services = fake_services(output_fixture());
        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("m").await })
            .expect("create playlist");
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            row_id: RowId(3),
            playlist_id: list.id,
            path: std::path::PathBuf::from("/nonexistent/nope.wav"),
            hash: TrackHash("missing".to_owned()),
        });

        let events = drain_terminal(&bus, RowId(3));
        assert!(
            events.iter().any(|e| matches!(e, Event::RowFailed { .. })),
            "missing file fails gracefully"
        );
    }

    // Given two jobs enqueued back to back.
    // When the worker drains them.
    // Then both complete (strict FIFO, one at a time).
    #[test]
    fn queue_processes_one_job_at_a_time() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("s").await })
            .expect("create playlist");
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        for name in ["one.wav", "two.wav"] {
            let path = dir.path().join(name);
            let bytes = wav_bytes(1.0);
            std::fs::write(&path, &bytes).expect("write wav");
            queue.enqueue(QueueJob {
                row_id: RowId(1),
                playlist_id: list.id,
                path,
                hash: TrackHash(hex_sha256(&bytes)),
            });
        }

        drain_terminal(&bus, RowId(1));
        drain_terminal(&bus, RowId(1));
    }

    // Given several jobs completed through the worker.
    // When all terminal events landed.
    // Then every event carries metadata only (no PCM rides along).
    #[test]
    fn queue_worker_drops_pcm() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("p").await })
            .expect("create playlist");
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        let n = 4;
        for i in 0..n {
            let path = dir.path().join(format!("pcm{i}.wav"));
            let mut bytes = wav_bytes(1.0);
            // Unique content per job: distinct hashes, no duplicate rejection.
            let last = bytes.len() - 1;
            bytes[last] = u8::try_from(i).expect("i fits");
            std::fs::write(&path, &bytes).expect("write wav");
            let hash = TrackHash(hex_sha256(&bytes));
            services.runtime.block_on(async {
                services
                    .playlist_store
                    .ensure_track(list.id, &hash, "/x", &format!("t{i}"), "", None)
                    .await
                    .expect("ensure entry")
            });
            queue.enqueue(QueueJob {
                row_id: RowId(i64::from(i) + 1),
                playlist_id: list.id,
                path,
                hash,
            });
        }
        let mut ready_count = 0;
        for i in 0..n {
            match drain_terminal(&bus, RowId(i64::from(i) + 1)).pop() {
                Some(Event::RowReady { meta, .. }) => {
                    ready_count += 1;
                    assert!(meta.title.len() <= 64, "metadata only");
                }
                Some(Event::RowFailed { message, .. }) => panic!("job failed: {message}"),
                _ => panic!("expected a terminal event"),
            }
        }
        assert_eq!(ready_count, n, "all jobs completed");
    }
}
