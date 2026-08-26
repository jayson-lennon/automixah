//! The analysis queue: a single worker draining track jobs by priority.
//!
//! One job at a time (analysis is CPU-heavy; parallel jobs would starve
//! the host). Jobs are addressed by content hash — the same stable
//! identity the stores and the track database use — so one analysis pass
//! serves every playlist row referencing the hash. Jobs are scheduled by
//! tier ([`Priority`]: user-forced above playlist above background
//! library backfill), FIFO within a tier; hash duplicates collapse onto
//! the already-staged job instead of running twice. The worker either
//! short-circuits on a library hit (grid + key + duration known → no
//! decode) or reads, decodes, analyzes through the injected analyzer,
//! persists grid/key, and drops the PCM. All outcomes are bus events
//! addressed by hash.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use automixah_engine::timeline::types::TrackHash;
use parking_lot::{Condvar, Mutex};

use crate::bus::Event;
use crate::services::Services;
use crate::track::identity;
use crate::tracks::{Analysis, AnalysisState};

/// Scheduling tier of a queued job; lower ranks dequeue sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// User-initiated re-analyze: jumps every pending background work.
    Force,
    /// User-visible playlist flow (adds, tag hydration, loads).
    Playlist,
    /// Library-refresh backfill: pure prefetch behind all user work.
    Library,
}

impl Priority {
    /// Numeric rank for the heap order (ascending = soonest).
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Force => 0,
            Self::Playlist => 1,
            Self::Library => 2,
        }
    }
}

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

/// A staged job plus its scheduling metadata.
#[derive(Debug)]
struct Pending {
    job: QueueJob,
    priority: Priority,
    /// Monotonic arrival sequence: FIFO within a tier, older wins ties.
    arrival: u64,
}

/// Pop order: tier rank first, then arrival sequence.
type Slot = (u8, u64);

/// State guarded by the pair (`Mutex`, `Condvar`) on [`Shared`].
#[derive(Debug, Default)]
struct Slots {
    /// Staged jobs keyed by content hash — never two entries per hash.
    staged: HashMap<TrackHash, Pending>,
    /// Pop-ordered view over `staged`: `(tier, arrival)` slots.
    ready: BTreeSet<Slot>,
    next_arrival: u64,
    /// Set when the owning handle drops: no further enqueues, worker
    /// exits after draining.
    closed: bool,
}

impl Slots {
    /// Stages `job`, collapsing duplicate hashes onto one execution —
    /// urgency escalates, `force` merges, the older arrival sequence
    /// survives so FIFO order holds within the merged tier. Returns
    /// whether the worker should be woken (a fresh, unseen arrival).
    fn offer(&mut self, job: QueueJob, priority: Priority) -> bool {
        let hash = job.hash.clone();
        let Some(mut merged) = self.staged.remove(&hash) else {
            // Fresh arrival.
            let arrival = self.next_arrival;
            self.next_arrival += 1;
            self.ready.insert((priority.rank(), arrival));
            self.staged.insert(
                hash,
                Pending {
                    job,
                    priority,
                    arrival,
                },
            );
            return true;
        };
        merged.job.force |= job.force;
        if priority >= merged.priority {
            self.staged.insert(hash, merged);
            return false;
        }
        // Escalation: relocate onto the more urgent slot, retaining the
        // original arrival sequence.
        self.ready.remove(&(merged.priority.rank(), merged.arrival));
        self.ready.insert((priority.rank(), merged.arrival));
        merged.priority = priority;
        self.staged.insert(hash, merged);
        false
    }

    /// Removes and returns the soonest staged job. Dequeue scans the
    /// staged map once for the popped slot's arrival sequence — dequeues
    /// are rare relative to runtime, staging stays O(log n) either way.
    fn pop_top(&mut self) -> Option<QueueJob> {
        let slot @ (_, arrival) = *self.ready.iter().next()?;
        self.ready.remove(&slot);
        // Every slot in `ready` matches exactly one staged job.
        let (_, pending) = self
            .staged
            .extract_if(|_, pending| pending.arrival == arrival)
            .next()?;
        Some(pending.job)
    }
}

/// Shared between the [`AnalysisQueue`] handle clones and the worker.
#[derive(Debug, Default)]
struct Shared {
    slots: Mutex<Slots>,
    space: Condvar,
}

/// Handle to the analysis worker: enqueue jobs, clone to share; the
/// worker sends bus events.
#[derive(Debug, Clone)]
pub struct AnalysisQueue {
    shared: Arc<Shared>,
}

impl AnalysisQueue {
    /// Spawns the single worker thread over `services`.
    ///
    /// The worker parks when idle; dropping the last frontend handle
    /// closes the queue — no further enqueues — and the worker exits
    /// once the backlog drains (app teardown).
    #[must_use]
    pub fn spawn(services: Services, events: Sender<Event>) -> Self {
        let shared = Arc::new(Shared::default());
        {
            let worker_shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("playlist-analysis".to_owned())
                .spawn(move || worker_loop(services, worker_shared, events))
                .expect("spawn analysis worker");
        }
        Self { shared }
    }

    /// Enqueues a job at `priority`; the track's record should already
    /// show `Queued`.
    ///
    /// A duplicate hash collapses onto the already-staged job — urgency
    /// escalates to the more urgent tier and `force` merges in, with one
    /// execution per hash. A late enqueue after close is a silent no-op
    /// (records stay queued harmlessly), matching the old failed-send
    /// behavior.
    pub fn enqueue(&self, job: QueueJob, priority: Priority) {
        let mut slots = self.shared.slots.lock();
        if slots.closed {
            return;
        }
        if slots.offer(job, priority) {
            self.shared.space.notify_one();
        }
    }
}

impl Drop for AnalysisQueue {
    fn drop(&mut self) {
        // Only this handle can set `closed`; the worker clone never does.
        let mut slots = self.shared.slots.lock();
        slots.closed = true;
        self.shared.space.notify_all();
    }
}

/// The worker loop: one job at a time, most urgent tier first, FIFO
/// within a tier, until closed with an empty backlog.
fn worker_loop(services: Services, shared: Arc<Shared>, events: Sender<Event>) {
    while let Some(job) = {
        let mut slots = shared.slots.lock();
        loop {
            if let Some(job) = slots.pop_top() {
                break Some(job);
            }
            if slots.closed {
                break None;
            }
            shared.space.wait(&mut slots);
        }
    } {
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
    use crate::bus::EventBus;
    use crate::playlist::store::PlaylistStoreService;
    use crate::playlist::store::in_memory::InMemoryPlaylistStore;
    use crate::services::{AppPaths, Services};
    use crate::store::GridStoreService;
    use crate::store::in_memory::InMemoryGridStore;
    use automixah_engine::timeline::types::CueKind;
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
            library_store: crate::library::store::LibraryStoreService::new(std::sync::Arc::new(
                crate::library::store::in_memory::InMemoryLibraryStore::new(),
            )),
            scan_latch: std::sync::Arc::default(),
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
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path,
                force: false,
            },
            Priority::Playlist,
        );

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
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path,
                force: false,
            },
            Priority::Playlist,
        );

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
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path: std::path::PathBuf::from("/nonexistent/nope.wav"),
                force: false,
            },
            Priority::Playlist,
        );

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
            queue.enqueue(
                QueueJob {
                    hash,
                    path,
                    force: false,
                },
                Priority::Playlist,
            );
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
            queue.enqueue(
                QueueJob {
                    hash,
                    path,
                    force: false,
                },
                Priority::Playlist,
            );
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
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path: path.clone(),
                force: false,
            },
            Priority::Playlist,
        );
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
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path,
                force: true,
            },
            Priority::Force,
        );
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

    fn slot_job(hash: &str) -> QueueJob {
        QueueJob {
            hash: TrackHash(hash.to_owned()),
            path: PathBuf::from(format!("/{hash}.wav")),
            force: false,
        }
    }

    /// Drains the ordering core deterministically: pop order until empty.
    fn drain_order(slots: &mut Slots) -> Vec<String> {
        let mut order = Vec::new();
        while let Some(job) = slots.pop_top() {
            order.push(job.hash.0);
        }
        order
    }

    // Given a Library-tier backlog staged first and a Playlist job after.
    // When the queue drains.
    // Then the playlist job dequeues before every pending library job.
    #[test]
    fn prio_pop_orders_playlist_over_library_backlog() {
        let mut slots = Slots::default();
        for i in 0..3 {
            slots.offer(slot_job(&format!("lib{i}")), Priority::Library);
        }
        slots.offer(slot_job("urgent"), Priority::Playlist);

        let order = drain_order(&mut slots);

        assert_eq!(
            order.first().map(String::as_str),
            Some("urgent"),
            "playlist tier jumps the library backlog"
        );
        assert_eq!(order[1..], ["lib0", "lib1", "lib2"]);
    }

    // Given same-tier jobs staged in a known arrival order.
    // When the queue drains.
    // Then arrival sequence is preserved (FIFO within the tier).
    #[test]
    fn equal_priority_preserves_fifo_arrival() {
        let mut slots = Slots::default();
        for name in ["first", "second", "third"] {
            slots.offer(slot_job(name), Priority::Playlist);
        }

        let order = drain_order(&mut slots);

        assert_eq!(order, ["first", "second", "third"]);
    }

    // Given a hash staged at Library priority and re-enqueued at
    // Playlist priority (plus an unrelated fresh job between).
    // When the queue drains.
    // Then exactly one execution happens, positioned with the playlist
    // jobs ahead of remaining library work.
    #[test]
    fn duplicate_enqueue_escalates_priority_once() {
        let mut slots = Slots::default();
        for i in 0..2 {
            slots.offer(slot_job(&format!("lib{i}")), Priority::Library);
        }
        slots.offer(slot_job("twin"), Priority::Library);
        slots.offer(slot_job("other"), Priority::Playlist);
        let woken = slots.offer(slot_job("twin"), Priority::Playlist);
        assert!(!woken, "duplicate must not wake the worker");

        let order = drain_order(&mut slots);

        let executions = order.iter().filter(|h| *h == "twin").count();
        assert_eq!(executions, 1, "one execution per hash");
        let twin_index = order.iter().position(|h| h == "twin").expect("ran");
        let lib1_index = order.iter().position(|h| h == "lib1").expect("runs");
        assert!(
            twin_index < lib1_index,
            "escalated twin runs before leftover library work: {order:?}"
        );
    }

    // Given a queued non-forced duplicate arriving for a staged forced
    // re-analyze job.
    // When both merge onto one entry.
    // Then the merged job keeps force semantics and deletes + re-analyzes.
    #[test]
    fn force_merge_keeps_reanalyze_semantics() {
        let services = fake_services(output_fixture());
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("merged.wav");
        let bytes = wav_bytes(2.0);
        std::fs::write(&path, &bytes).expect("write wav");
        let hash = TrackHash(identity::hex_sha256(&bytes));

        // First pass persists grid + key so a fast path exists.
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path: path.clone(),
                force: false,
            },
            Priority::Playlist,
        );
        drain_terminal(&bus, &hash);
        let stored_before = services
            .runtime
            .block_on(async { services.grid_store.get(&hash).await })
            .expect("lookup")
            .expect("grid persisted by first pass");

        // Merge test: forced job stages while a plain Library-priority
        // twin is already pending — one merged execution total.
        let merged_slots = {
            let mut slots = Slots::default();
            slots.offer(
                QueueJob {
                    hash: hash.clone(),
                    path: path.clone(),
                    force: false,
                },
                Priority::Library,
            );
            slots.offer(
                QueueJob {
                    hash: hash.clone(),
                    path: path.clone(),
                    force: true,
                },
                Priority::Force,
            );
            slots
        };
        let drained = drain_order(&mut { merged_slots });
        assert_eq!(drained.len(), 1, "single merged execution");

        // The real worker with the merged shape deletes the stored grid
        // first and analyzes fresh — exercised end to end via Force.
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services.clone(), bus.sender());
        queue.enqueue(
            QueueJob {
                hash: hash.clone(),
                path,
                force: true,
            },
            Priority::Force,
        );
        let events = drain_terminal(&bus, &hash);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::AnalysisStarted { .. })),
            "forced merge re-runs analysis"
        );
        let stored_after = services
            .runtime
            .block_on(async { services.grid_store.get(&hash).await })
            .expect("lookup")
            .expect("fresh grid persisted");
        assert!(stored_after.updated_at >= stored_before.updated_at);
        assert!(
            stored_after.key.is_some(),
            "fast path cannot serve a forced job"
        );
    }

    // Given two handles on one queue and the first handle dropped
    // (closing it).
    // When a job arrives through the stale surviving clone.
    // Then the enqueue is accepted silently as a no-op — records stay
    // queued harmlessly, nothing panics (the app-teardown contract).
    #[test]
    fn late_enqueue_after_close_is_silent_noop() {
        let services = fake_services(output_fixture());
        let bus = EventBus::without_repaint();
        let queue = AnalysisQueue::spawn(services, bus.sender());
        let stale_clone = queue.clone();
        drop(queue);

        stale_clone.enqueue(slot_job("late"), Priority::Library);
    }
}
