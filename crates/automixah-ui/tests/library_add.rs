//! Library add-flow integration: double-click intent → store insert →
//! bus events, without touching the filesystem (every fact comes from
//! the seeded library index).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use automixah_engine::timeline::types::TrackHash;
use automixah_ui_lib::app::AutomixahUiApp;
use automixah_ui_lib::bus::{Event, EventBus};
use automixah_ui_lib::library::store::{LibraryEntry, LibraryRoot};
use automixah_ui_lib::playlist::store::PlaylistStoreService;
use automixah_ui_lib::playlist::store::in_memory::InMemoryPlaylistStore;
use automixah_ui_lib::services::{AppPaths, Services};
use automixah_ui_lib::store::{CueStoreService, GridStoreService};

fn test_services() -> (Services, PlaylistStoreService) {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime"),
    );
    let playlist_store = PlaylistStoreService::new(Arc::new(InMemoryPlaylistStore::new()));
    let services = Services {
        paths: AppPaths::for_test(Path::new("/tmp/unused")),
        grid_store: GridStoreService::new(Arc::new(
            automixah_ui_lib::store::in_memory::InMemoryGridStore::new(),
        )),
        cue_store: CueStoreService::new(Arc::new(
            automixah_ui_lib::store::in_memory::InMemoryCueStore::new(),
        )),
        playlist_store: playlist_store.clone(),
        library_store: automixah_ui_lib::library::store::LibraryStoreService::new(Arc::new(
            automixah_ui_lib::library::store::in_memory::InMemoryLibraryStore::new(),
        )),
        scan_latch: std::sync::Arc::default(),
        analyzer: Arc::new(djcore::analyzer::FakeAnalyzer::with_output(
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
        )),
        runtime,
    };
    (services, playlist_store)
}

fn seeded_entry(hash: &str) -> LibraryEntry {
    LibraryEntry {
        root_id: 1,
        rel_path: PathBuf::from("sets/one.flac"),
        hash: TrackHash(hash.to_owned()),
        title: "One".to_owned(),
        artist: "Artist".to_owned(),
        duration: Some(61.0),
        bpm: None,
        key: None,
        mtime_secs: 0,
        size_bytes: 0,
    }
}

fn seeded_app(services: Services) -> AutomixahUiApp {
    let bus = EventBus::without_repaint();
    let mut app = AutomixahUiApp::new(services, bus);
    app.seed_library_for_test(
        vec![LibraryRoot {
            id: 1,
            path: PathBuf::from("/music"),
        }],
        vec![seeded_entry("h1")],
    );
    app
}

// Given an app with a selected playlist and a seeded library.
// When a library track is added.
// Then TagsResolved + RowAdded land on the bus and the store holds the
// row with the index's facts.
#[test]
fn add_library_track_persists_and_reports() {
    let (services, playlist_store) = test_services();
    let runtime = services.runtime.clone();
    let list = runtime
        .block_on(async { playlist_store.create_playlist("mix").await })
        .expect("playlist");
    let mut app = seeded_app(services);
    app.select_playlist_for_test(list.id);

    app.add_library_track_for_test(TrackHash("h1".to_owned()));

    // Let the spawned task finish, then collect terminal events.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let mut saw_tags = false;
    let mut saw_row = false;
    while let Ok(event) = app.bus_receiver_for_test().try_recv() {
        match event {
            Event::TagsResolved { hash, tags } => {
                saw_tags = true;
                assert_eq!(hash.0, "h1");
                assert_eq!(tags.title, "One");
                assert_eq!(tags.path, PathBuf::from("/music/sets/one.flac"));
            }
            Event::RowAdded { playlist_id, hash } => {
                saw_row = true;
                assert_eq!(playlist_id, list.id);
                assert_eq!(hash.0, "h1");
            }
            Event::AddStarted { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(saw_tags, "TagsResolved landed");
    assert!(saw_row, "RowAdded landed");
    let rows = runtime
        .block_on(async { playlist_store.tracks_for(list.id).await })
        .expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "One");
    assert_eq!(rows[0].duration, Some(61.0));
    assert_eq!(rows[0].added_path, "/music/sets/one.flac");
}

// Given an app whose selected playlist already contains the hash.
// When the same library track is added again.
// Then the duplicate is skipped (no RowAdded).
#[test]
fn add_library_track_duplicate_is_skipped() {
    let (services, playlist_store) = test_services();
    let runtime = services.runtime.clone();
    let list = runtime
        .block_on(async { playlist_store.create_playlist("mix").await })
        .expect("playlist");
    runtime
        .block_on(async {
            playlist_store
                .insert_track(
                    list.id,
                    &TrackHash("h1".to_owned()),
                    "/x",
                    "One",
                    "Artist",
                    None,
                )
                .await
        })
        .expect("seed row");
    let mut app = seeded_app(services);
    app.select_playlist_for_test(list.id);

    app.add_library_track_for_test(TrackHash("h1".to_owned()));

    std::thread::sleep(std::time::Duration::from_millis(100));
    let mut saw_duplicate = false;
    while let Ok(event) = app.bus_receiver_for_test().try_recv() {
        match event {
            Event::DuplicateSkipped { .. } => saw_duplicate = true,
            Event::RowAdded { .. } => panic!("duplicate re-added"),
            Event::AddStarted { .. } => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(saw_duplicate);
}

// Given a selected playlist that already holds two hydrated rows.
// When a third track is added from the library.
// Then the store APPENDS (existing rows keep their facts, the new row
// carries the index's title/artist) — a regression guard for the
// reported "double-click replaces the playlist with a blank entry".
#[test]
fn add_appends_and_preserves_existing_rows() {
    let (services, playlist_store) = test_services();
    let runtime = services.runtime.clone();
    let list = runtime
        .block_on(async { playlist_store.create_playlist("mix").await })
        .expect("playlist");
    // Pre-existing rows inserted directly through the store.
    let existing: Vec<automixah_ui_lib::playlist::store::PersistedTrack> =
        runtime.block_on(async {
            playlist_store
                .insert_track(
                    list.id,
                    &TrackHash("old1".to_owned()),
                    "/a",
                    "Old One",
                    "A",
                    None,
                )
                .await
                .expect("insert old1");
            playlist_store
                .insert_track(
                    list.id,
                    &TrackHash("old2".to_owned()),
                    "/b",
                    "Old Two",
                    "B",
                    None,
                )
                .await
                .expect("insert old2");
            playlist_store.tracks_for(list.id).await.expect("rows")
        });
    assert_eq!(existing.len(), 2);

    let mut app = seeded_app(services);
    // The library seeds h1; add it through the double-click path.
    app.select_playlist_for_test(list.id);
    app.add_library_track_for_test(TrackHash("h1".to_owned()));
    std::thread::sleep(std::time::Duration::from_millis(100));
    let rx = app.bus_receiver_for_test();
    while rx.try_recv().is_ok() {}

    let rows = runtime
        .block_on(async { playlist_store.tracks_for(list.id).await })
        .expect("rows");
    assert_eq!(rows.len(), 3, "append, never replace");
    assert_eq!(rows[0].title, "Old One");
    assert_eq!(rows[0].track_hash.0, "old1");
    assert_eq!(rows[2].title, "One");
    assert_eq!(rows[2].artist, "Artist");
}
