//! Library scanner integration tests: real fixture files in tempdir
//! roots, scanned through the in-memory store against the same bus
//! dialect the UI consumes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use automixah_ui_lib::bus::Event;
use automixah_ui_lib::library::scan::{self, ScanOutcome};
use automixah_ui_lib::library::store::LibraryStoreService;
use automixah_ui_lib::library::store::in_memory::InMemoryLibraryStore;
use automixah_ui_lib::services::{AppPaths, Services};
use automixah_ui_lib::store::CueStoreService;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/{name}");
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
}

/// A services bundle with an in-memory library store and a tempdir root
/// holding the given fixture files. Sync: the runtime drops outside any
/// async context.
fn services_with_root(files: &[&str]) -> (Services, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    for name in files {
        std::fs::write(dir.path().join(name), fixture(name)).expect("write fixture");
    }
    let root = dir.path().to_path_buf();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime"),
    );
    let library_store = LibraryStoreService::new(Arc::new(InMemoryLibraryStore::new()));
    let services = Services {
        paths: AppPaths::for_test(Path::new("/tmp/unused")),
        grid_store: automixah_ui_lib::store::GridStoreService::new(Arc::new(
            automixah_ui_lib::store::in_memory::InMemoryGridStore::new(),
        )),
        cue_store: CueStoreService::new(Arc::new(
            automixah_ui_lib::store::in_memory::InMemoryCueStore::new(),
        )),
        playlist_store: automixah_ui_lib::playlist::store::PlaylistStoreService::new(Arc::new(
            automixah_ui_lib::playlist::store::in_memory::InMemoryPlaylistStore::new(),
        )),
        library_store,
        scan_latch: std::sync::Arc::default(),
        analyzer: Arc::new(djcore::analyzer::FakeAnalyzer::with_output(fake_output())),
        runtime,
    };
    services
        .runtime
        .block_on(async {
            services
                .library_store
                .add_root(&root.display().to_string())
                .await
        })
        .expect("add root");
    (services, root, dir)
}

/// Minimal fake analysis output for the services bundle.
fn fake_output() -> djcore::analyzer::AnalyzerOutput {
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
    }
}

/// A progress monitor wired to a local channel; `drain` collects the
/// `(done, seen)` echoes it sent.
fn recorder() -> (scan::ProgressMonitor, std::sync::mpsc::Receiver<Event>) {
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    (scan::ProgressMonitor::new(tx), rx)
}

/// Collects monitor echoes from the recorder's channel.
fn echoes_of(rx: &std::sync::mpsc::Receiver<Event>) -> Vec<(usize, usize)> {
    rx.try_iter()
        .filter_map(|event| match event {
            Event::LibraryScanProgress {
                files_done,
                files_seen,
            } => Some((files_done, files_seen)),
            _ => None,
        })
        .collect()
}

/// Runs one scan on the services runtime (sync wrapper), recording
/// progress echoes.
fn scan_once(services: &Services) -> ScanOutcome {
    let (progress, rx) = recorder();
    let outcome = services
        .runtime
        .block_on(async { scan::scan(services, progress).await })
        .expect("scan");
    SCAN_ECHOES.with(|slot| slot.borrow_mut().extend(echoes_of(&rx)));
    outcome
}

thread_local! {
    /// Progress echoes of the most recent scan_once (test assertions).
    static SCAN_ECHOES: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Forces a distinct mtime on a path so the change check notices edits.
fn touch_later(path: &Path) {
    let future = std::time::SystemTime::now() + Duration::from_secs(60);
    let file_time = filetime::FileTime::from_system_time(future);
    filetime::set_file_mtime(path, file_time).expect("set mtime");
}

/// Lists entries on the services runtime (sync wrapper).
fn list_entries(services: &Services) -> Vec<automixah_ui_lib::library::store::LibraryEntry> {
    services
        .runtime
        .block_on(async { services.library_store.list_entries().await })
        .expect("entries")
}

// Given a root with supported and unsupported files.
// When scanned.
// Then only supported audio files are indexed, with real hashes.
#[test]
fn scan_indexes_supported_files_only() {
    let (services, root, _dir) =
        services_with_root(&["tone440.wav", "tone440.flac", "tone440.mp3"]);
    std::fs::write(root.join("notes.txt"), "not audio").expect("write junk");

    let outcome = scan_once(&services);

    assert_eq!(
        outcome,
        ScanOutcome {
            added: 3,
            updated: 0,
            pruned: 0
        }
    );
    let entries = list_entries(&services);
    assert_eq!(entries.len(), 3, "only supported extensions indexed");
    let expected_hash = automixah_ui_lib::track::identity::hex_sha256(&fixture("tone440.wav"));
    assert!(
        entries
            .iter()
            .any(|e| e.hash.0 == expected_hash && e.rel_path == *"tone440.wav"),
        "wav entry carries the real content hash"
    );
}

// Given an indexed root.
// When scanned again with no changes.
// Then no file is re-read (added + updated stay zero).
#[test]
fn incremental_scan_skips_unchanged_files() {
    let (services, _root, _dir) = services_with_root(&["tone440.wav", "tone440.flac"]);
    scan_once(&services);

    let outcome = scan_once(&services);

    assert_eq!(outcome, ScanOutcome::default(), "nothing re-read");
}

// Given an indexed root.
// When one file is deleted and another is renamed.
// Then the vanished file is pruned and the moved file re-indexes under
// its new name with the same hash.
#[test]
fn scan_prunes_vanished_and_tracks_moves() {
    let (services, root, _dir) = services_with_root(&["tone440.wav", "tone440.flac"]);
    scan_once(&services);
    std::fs::remove_file(root.join("tone440.wav")).expect("delete");
    std::fs::rename(root.join("tone440.flac"), root.join("moved.flac")).expect("rename");
    touch_later(&root.join("moved.flac"));

    let outcome = scan_once(&services);

    assert_eq!(
        outcome,
        ScanOutcome {
            added: 1,
            updated: 0,
            pruned: 2
        },
        "old name pruned plus the vanished wav; new name added"
    );
    let entries = list_entries(&services);
    assert_eq!(entries.len(), 1);
    let flac_hash = automixah_ui_lib::track::identity::hex_sha256(&fixture("tone440.flac"));
    assert_eq!(entries[0].rel_path, PathBuf::from("moved.flac"));
    assert_eq!(entries[0].hash.0, flac_hash, "same content, same hash");
}

// Given an indexed file.
// When its bytes change on disk.
// Then the next scan re-reads and updates the row.
#[test]
fn scan_updates_changed_files() {
    let (services, root, _dir) = services_with_root(&["tone440.wav"]);
    scan_once(&services);
    // Same size is fine: mtime changes because we touch the file later.
    std::fs::write(root.join("tone440.wav"), fixture("tone440.flac")).expect("swap bytes");
    touch_later(&root.join("tone440.wav"));

    let outcome = scan_once(&services);

    assert_eq!(
        outcome,
        ScanOutcome {
            added: 0,
            updated: 1,
            pruned: 0
        }
    );
    let entries = list_entries(&services);
    let flac_hash = automixah_ui_lib::track::identity::hex_sha256(&fixture("tone440.flac"));
    assert_eq!(entries[0].hash.0, flac_hash, "hash follows the new bytes");
}

// Given a root with supported files in nested subdirectories.
// When scanned.
// Then every supported file is indexed at its relative path, whatever
// the walk order.
#[test]
fn scan_recurses_into_subdirectories() {
    let (services, root, _dir) = services_with_root(&["tone440.wav"]);
    std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
    std::fs::write(root.join("a/tone440.flac"), fixture("tone440.flac")).expect("write flac");
    std::fs::write(root.join("a/b/tone440.mp3"), fixture("tone440.mp3")).expect("write mp3");

    let outcome = scan_once(&services);

    assert_eq!(outcome.added, 3);
    let rel_paths: Vec<PathBuf> = {
        let mut paths: Vec<PathBuf> = list_entries(&services)
            .into_iter()
            .map(|e| e.rel_path)
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(
        rel_paths,
        vec![
            PathBuf::from("a/b/tone440.mp3"),
            PathBuf::from("a/tone440.flac"),
            PathBuf::from("tone440.wav"),
        ],
        "nested files indexed under their relative paths"
    );
}

// Given a scanned root with an indexed file.
// When load_library runs.
// Then the persisted roots and entries round-trip.
#[test]
fn load_library_returns_persisted_state() {
    let (services, _root, _dir) = services_with_root(&["tone440.wav"]);
    scan_once(&services);

    let (roots, entries) = services
        .runtime
        .block_on(async { scan::load_library(&services.library_store).await })
        .expect("load");

    assert_eq!(roots.len(), 1);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].rel_path, PathBuf::from("tone440.wav"));
}

// Given a scan over several files.
// When progress echoes arrive.
// Then the final echo reports every file done and seen.
#[test]
fn scan_reports_progress_per_batch() {
    let (services, _root, _dir) =
        services_with_root(&["tone440.wav", "tone440.flac", "tone440.mp3"]);
    let (progress, rx) = recorder();
    services
        .runtime
        .block_on(async { scan::scan(&services, progress).await })
        .expect("scan");

    let echoes = echoes_of(&rx);
    assert!(
        echoes.iter().all(|(done, seen)| done <= seen),
        "done never exceeds discovered"
    );
    assert_eq!(echoes.last(), Some(&(3, 3)), "final echo is complete");
}

// Given a root with files and a scan already spawned.
// When spawn_scan is called again before the first finishes.
// Then scans stay serial but the request is honored: two sequential
// Started/Done pairs (the second covers roots added mid-scan), and
// `seen` never exceeds the number of files per scan.
#[test]
fn concurrent_spawn_scan_queues_one_followup() {
    let (services, _root, _dir) =
        services_with_root(&["tone440.wav", "tone440.flac", "tone440.mp3"]);
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    scan::spawn_scan(&services, tx.clone());
    scan::spawn_scan(&services, tx);

    // Drive the runtime until both scans finish.
    services.runtime.block_on(async {
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut started = 0;
    let mut done = 0;
    let mut max_seen = 0;
    while let Ok(event) = rx.try_recv() {
        match event {
            Event::LibraryScanStarted => started += 1,
            Event::LibraryScanProgress { files_seen, .. } => {
                max_seen = max_seen.max(files_seen);
            }
            Event::LibraryScanDone { .. } => done += 1,
            _ => {}
        }
    }
    assert_eq!(started, 2, "dropped spawn queued a follow-up scan");
    assert_eq!(done, 2);
    assert_eq!(max_seen, 3, "seen counts each file exactly once per scan");
}

// Given more than one batch worth of files (BATCH_STEP is 100).
// When scanned.
// Then commits are batched: intermediate echoes fire at batch boundaries
// and the final state holds every file.
#[test]
fn scan_batches_commits_for_large_roots() {
    let (services, root, _dir) = services_with_root(&["tone440.wav"]);
    // 150 extra files over the 100-file batch boundary.
    for i in 0..150 {
        std::fs::write(root.join(format!("copy{i}.wav")), fixture("tone440.wav"))
            .expect("write copy");
    }
    let (progress, rx) = recorder();
    let outcome = services
        .runtime
        .block_on(async { scan::scan(&services, progress).await })
        .expect("scan");

    assert_eq!(outcome.added, 151);
    let echoes = echoes_of(&rx);
    // Per-file done reporting: the 100-boundary fired mid-scan. The
    // walker races ahead of the classifier, so `seen` at that moment is
    // timing-dependent — only `done` is pinned.
    assert!(
        echoes.iter().any(|(done, _)| *done == 100),
        "batch boundary echo"
    );
    assert_eq!(echoes.last(), Some(&(151, 151)));
    assert_eq!(list_entries(&services).len(), 151, "every file committed");
}
