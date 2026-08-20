//! The analysis queue: a single worker draining FIFO track jobs.
//!
//! One job at a time (analysis is CPU-heavy; parallel jobs would starve
//! the host). Jobs are addressed by content hash — the same stable
//! identity the stores and the track database use — so one analysis pass
//! serves every playlist row referencing the hash. The worker either
//! short-circuits on a library hit (grid + key + duration known → no
//! decode) or reads, decodes, analyzes through the injected analyzer,
//! persists grid/key, and drops the PCM. All outcomes are bus events
//! addressed by hash.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use automixah_engine::timeline::types::TrackHash;

use crate::bus::Event;
use crate::services::Services;
use crate::track::identity;
use crate::tracks::{Analysis, AnalysisState};

/// One queued analysis job.
#[derive(Debug, Clone)]
pub struct QueueJob {
    /// Track to analyze (jobs and events address tracks by hash).
    pub hash: TrackHash,
    /// Path of the audio file.
    pub path: PathBuf,
    /// `true` for re-analyze: delete the stored grid first (ordered
    /// before any store read, inside this worker) and skip the library
    /// fast path so analysis always actually runs.
    pub force: bool,
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

    /// Enqueues a job; the track's record should already show `Queued`.
    pub fn enqueue(&self, job: QueueJob) {
        // A send fails only when the worker is gone (app teardown); the
        // record then stays queued harmlessly.
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
        let _ = events.send(Event::AnalysisFailed {
            hash: job.hash.clone(),
            message,
        });
    };
    let send = |event: Event| {
        let _ = events.send(event);
    };

    // Forced (re-analyze): the persisted grid dies first, ordered
    // before any store read in this worker — the stale fast path
    // cannot race by construction.
    if job.force {
        let store = services.grid_store.clone();
        let hash = job.hash.clone();
        if let Err(report) = services
            .runtime
            .handle()
            .block_on(async { store.delete(&hash).await })
        {
            return fail(format!("delete stored grid: {report:#}"));
        }
    }

    // Library fast path: grid + key + duration all known → no decode.
    // A duplicate job for an already-analyzed hash lands here cheaply;
    // a forced job never lands here (its row was just deleted).
    if !job.force
        && let Some(analysis) = library_hit(services, &job.hash)
    {
        send(Event::AnalysisDone {
            hash: job.hash.clone(),
            analysis,
        });
        return;
    }

    send(Event::AnalysisStarted {
        hash: job.hash.clone(),
    });

    let bytes = match std::fs::read(&job.path) {
        Ok(bytes) => bytes,
        Err(e) => return fail(format!("read {}: {e}", job.path.display())),
    };

    let registry = djcore::decoder::DecoderRegistry::with_symphonia();
    let extension = identity::extension_of(&job.path);
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
                    updated_at: identity::now_unix(),
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
    let cues = runtime
        .block_on(async { services.cue_store.get(&hash).await })
        .ok()
        .unwrap_or_default();
    send(Event::AnalysisDone {
        hash,
        analysis: Analysis {
            grid: output.beat_grid,
            bpm: output.bpm,
            key: output.key,
            duration_seconds: output.duration_seconds,
            cues,
        },
    });
}

/// The library-hit fast path: the full analysis package from the stores,
/// no decode. Grid and key come from the stored `GridOverride`; duration
/// from the playlist store (the analyzer value written at first
/// analysis — the probe may legitimately know less).
fn library_hit(services: &Services, hash: &TrackHash) -> Option<Analysis> {
    let runtime = services.runtime.handle();
    let stored = runtime
        .block_on(async { services.grid_store.get(hash).await })
        .ok()??;
    let key = stored.key.clone()?;
    let duration = runtime
        .block_on(async { services.playlist_store.track_duration(hash).await })
        .ok()
        .flatten()?;
    let cues = runtime
        .block_on(async { services.cue_store.get(hash).await })
        .ok()
        .unwrap_or_default();
    #[expect(clippy::cast_possible_truncation, reason = "f64 store to f32 display")]
    Some(Analysis {
        grid: crate::grid::EditableGrid {
            grid_bpm: stored.grid_bpm,
            anchor_seconds: stored.anchor_seconds,
            downbeat_phase: stored.downbeat_phase,
        }
        .project(),
        bpm: stored.grid_bpm,
        key,
        duration_seconds: duration as f32,
        cues,
    })
}

/// The state the enqueue derivation sets before a job is sent.
#[must_use]
pub fn queued_state() -> AnalysisState {
    AnalysisState::Queued
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use automixah_engine::timeline::types::CueKind;
    use crate::bus::EventBus;
    use crate::playlist::store::PlaylistStoreService;
    use crate::playlist::store::in_memory::InMemoryPlaylistStore;
    use crate::services::{AppPaths, Services};
    use crate::store::GridStoreService;
    use crate::store::in_memory::InMemoryGridStore;
    use djcore::analyzer::{AnalyzerOutput, FakeAnalyzer};

    pub(crate) fn fake_services(analyzer_output: AnalyzerOutput) -> Services {
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
            cue_store: crate::store::CueStoreService::new(std::sync::Arc::new(
                crate::store::in_memory::InMemoryCueStore::new(),
            )),
            playlist_store: PlaylistStoreService::new(std::sync::Arc::new(
                InMemoryPlaylistStore::new(),
            )),
            analyzer: std::sync::Arc::new(FakeAnalyzer::with_output(analyzer_output)),
            runtime,
        }
    }

    pub(crate) fn output_fixture() -> AnalyzerOutput {
        AnalyzerOutput {
            bpm: 138.0,
            key: djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 2.0,
            beat_grid: djcore::analyzer::BeatGrid {
                grid_bpm: 138.0,
                ..Default::default()
            },
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

    /// Drains bus events until one terminal analysis event for `hash`
    /// lands (the worker emits started → terminal per job).
    fn drain_terminal(bus: &EventBus, hash: &TrackHash) -> Vec<Event> {
        let rx = bus.receiver_for_test();
        let mut events = Vec::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(event) => {
                    let terminal = matches!(
                        &event,
                        Event::AnalysisDone { hash: h, .. } | Event::AnalysisFailed { hash: h, .. } if h == hash
                    );
                    events.push(event);
                    if terminal {
                        return events;
                    }
                }
                Err(_) => return events,
            }
        }
    }

    // Given a wav file enqueued fresh.
    // When the worker runs the job.
    // Then the hash transitions through analyzing to done with an
    // analysis package, and the store holds the grid.
    #[test]
    fn queue_transitions_started_done_and_persists() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("Tenebrax - Impulse.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");

        // The add path persists first; the worker only analyzes.
        let hash = TrackHash(identity::hex_sha256(&bytes));
        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("q").await })
            .expect("create playlist");
        services.runtime.block_on(async {
            services
                .playlist_store
                .ensure_track(list.id, &hash, "/x", "Impulse", "Tenebrax", None)
                .await
                .expect("ensure entry");
        });
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            hash: hash.clone(),
            path,
            force: false,
        });

        let events = drain_terminal(&bus, &hash);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::AnalysisStarted { .. })),
            "started stage observed: {events:?}"
        );
        let analysis = events
            .into_iter()
            .find_map(|e| match e {
                Event::AnalysisDone { hash, analysis } => Some((hash, analysis)),
                _ => None,
            })
            .expect("done event");
        assert_eq!(analysis.0, hash, "event addresses the track by hash");

        let stored = services
            .runtime
            .block_on(async { services.grid_store.get(&hash).await })
            .expect("grid lookup")
            .expect("grid persisted");
        assert_eq!(stored.key.map(|k| k.root), Some(9), "key persisted");
    }

    // Given a hash with grid + key + duration already stored.
    // When the same file is enqueued.
    // Then the hash goes straight to done with no started stage, the
    // analyzer is never called, and the stored duration is reported.
    #[test]
    fn library_hit_skips_decode_and_reports_stored_duration() {
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
        let hash = TrackHash(identity::hex_sha256(&bytes));
        let list = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("q").await })
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
                .expect("seed tags");
        });
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            hash: hash.clone(),
            path,
            force: false,
        });

        let events = drain_terminal(&bus, &hash);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::AnalysisStarted { .. })),
            "no started stage on a library hit"
        );
        let analysis = events
            .into_iter()
            .find_map(|e| match e {
                Event::AnalysisDone { analysis, .. } => Some(analysis),
                _ => None,
            })
            .expect("done event");
        assert!(
            (analysis.duration_seconds - 122.25).abs() < 0.01,
            "stored duration"
        );
        assert_eq!(analyzer.call_count(), 0, "analyzer never called");
    }

    // Given an enqueued job whose file is missing.
    // When the worker runs it.
    // Then a failed event addresses the hash without panicking.
    #[test]
    fn missing_file_job_reports_failed_by_hash() {
        let services = fake_services(output_fixture());
        let hash = TrackHash("missing".to_owned());
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services, bus.sender());
        queue.enqueue(QueueJob {
            hash: hash.clone(),
            path: std::path::PathBuf::from("/nonexistent/nope.wav"),
            force: false,
        });

        let events = drain_terminal(&bus, &hash);
        assert!(
            events
                .iter()
                .any(|e| matches!(&e, Event::AnalysisFailed { hash: h, .. } if *h == hash)),
            "missing file fails gracefully by hash"
        );
    }

    // Given two jobs enqueued back to back.
    // When the worker drains them.
    // Then both complete (strict FIFO, one at a time).
    #[test]
    fn queue_processes_one_job_at_a_time() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services, bus.sender());
        for (i, name) in ["one.wav", "two.wav"].iter().enumerate() {
            let path = dir.path().join(name);
            let mut bytes = wav_bytes(1.0);
            // Unique content per job: distinct hashes, no library hits.
            let last = bytes.len() - 1;
            bytes[last] = u8::try_from(i).expect("i fits");
            std::fs::write(&path, &bytes).expect("write wav");
            let hash = TrackHash(identity::hex_sha256(&bytes));
            queue.enqueue(QueueJob {
                hash,
                path,
                force: false,
            });
        }

        // Two terminal events arrive eventually (both analyze; order kept).
        let rx = bus.receiver_for_test();
        let mut terminals = 0;
        while terminals < 2 {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(Event::AnalysisDone { .. }) | Ok(Event::AnalysisFailed { .. }) => {
                    terminals += 1;
                }
                Ok(_) => {}
                Err(_) => panic!("worker stalled"),
            }
        }
    }

    // Given a decoded job completing.
    // When the done event lands.
    // Then it carries metadata only (no PCM rides along).
    #[test]
    fn queue_worker_drops_pcm() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services, bus.sender());
        let n = 2;
        for i in 0..n {
            let path = dir.path().join(format!("pcm{i}.wav"));
            let mut bytes = wav_bytes(1.0);
            // Unique content per job: distinct hashes, no store collisions.
            let last = bytes.len() - 1;
            bytes[last] = u8::try_from(i).expect("i fits");
            std::fs::write(&path, &bytes).expect("write wav");
            let hash = TrackHash(identity::hex_sha256(&bytes));
            queue.enqueue(QueueJob {
                hash,
                path,
                force: false,
            });
        }
        let rx = bus.receiver_for_test();
        let mut done = 0;
        while done < n {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(Event::AnalysisDone { .. }) => done += 1,
                Ok(Event::AnalysisFailed { message, .. }) => panic!("job failed: {message}"),
                Ok(_) => {}
                Err(_) => panic!("worker stalled"),
            }
        }
    }

    // Given a hash whose grid was already persisted by a first job.
    // When a forced (re-analyze) job runs.
    // Then the stored grid is deleted before any read, analysis
    // actually re-runs (no library hit), and the fresh grid persists.
    #[test]
    fn forced_job_deletes_then_analyzes_fresh() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("re.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");
        let hash = TrackHash(identity::hex_sha256(&bytes));

        // First (non-forced) job analyzes and persists.
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            hash: hash.clone(),
            path: path.clone(),
            force: false,
        });
        drain_terminal(&bus, &hash);

        // A user in-cue persists for this hash.
        let seeded_cues = automixah_engine::timeline::types::CuePoints::with_in(0, 44_100);
        services
            .runtime
            .block_on(async { services.cue_store.put(&hash, &seeded_cues).await })
            .expect("seed cue");

        // Forced job: delete-before-read, fresh analysis.
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(QueueJob {
            hash: hash.clone(),
            path,
            force: true,
        });
        let events = drain_terminal(&bus, &hash);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::AnalysisStarted { .. })),
            "forced job re-runs analysis"
        );
        let stored = services
            .runtime
            .block_on(async { services.grid_store.get(&hash).await })
            .expect("lookup")
            .expect("fresh grid persisted");
        assert!(
            (stored.grid_bpm - 138.0).abs() < f32::EPSILON,
            "fake analyzer bpm"
        );
        // Cues live in a separate table, so forced re-analysis preserves them.
        let preserved = services
            .runtime
            .block_on(async { services.cue_store.get(&hash).await })
            .expect("cue lookup");
        assert_eq!(preserved.get(CueKind::In, 0), Some(44_100));
    }
}
