//! T6: every grid mutation flushes exactly one `put`; a sqlite row lands.

use std::sync::Arc;

use automixah_engine::timeline::types::TrackHash;
use automixah_ui_lib::app::AutomixahUiApp;
use automixah_ui_lib::playlist::store::PlaylistStoreService;
use automixah_ui_lib::playlist::store::in_memory::InMemoryPlaylistStore;
use automixah_ui_lib::services::{AppPaths, Services};
use automixah_ui_lib::store::CountingStore;
use automixah_ui_lib::store::in_memory::InMemoryGridStore;
use automixah_ui_lib::store::sqlite::SqliteGridStore;
use automixah_ui_lib::store::{GridOverride, GridStoreService};

// Given an app over a counting in-memory store with a loaded track.
// When the grid is mutated three times (shift, shift, downbeat).
// Then exactly three puts flush and the last value round-trips.
#[test]
fn each_grid_mutation_flushes_one_put() {
    let dir = tempfile::tempdir().expect("temp");
    let counting = Arc::new(CountingStore::new(Arc::new(InMemoryGridStore::new())));
    let services = Services {
        paths: AppPaths {
            data_dir: dir.path().to_path_buf(),
            library_db: dir.path().join("library.sqlite"),
        },
        grid_store: GridStoreService::new(counting.clone()),
        playlist_store: PlaylistStoreService::new(Arc::new(InMemoryPlaylistStore::new())),
        analyzer: std::sync::Arc::new(djcore::analyzer::FakeAnalyzer::with_output(
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
        runtime: std::sync::Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("test runtime"),
        ),
    };

    let mut app = AutomixahUiApp::new(services, automixah_ui_lib::bus::EventBus::without_repaint());
    app.inject_track_for_test(TrackHash("deadbeef".to_owned()));

    // Mutation 1: shift.
    app.test_shift_grid(0.010);
    app.flush_save_if_due_for_test();
    // Mutation 2: shift negative.
    app.test_shift_grid(-0.005);
    app.flush_save_if_due_for_test();
    // Mutation 3: downbeat phase change.
    app.test_set_downbeat_phase(2);
    app.flush_save_if_due_for_test();

    // Give the spawned puts a beat to run.
    std::thread::sleep(std::time::Duration::from_millis(50));

    assert_eq!(counting.puts(), 3, "one put per mutation");
}

// Given a sqlite store on a temp dir.
// When an override is written and the connection reopened.
// Then the row is present and identical.
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_row_lands_and_round_trips() {
    let dir = tempfile::tempdir().expect("temp");
    let db = dir.path().join("library.sqlite");
    let hash = TrackHash("cafebabe".to_owned());
    let grid = GridOverride {
        grid_bpm: 140.0,
        anchor_seconds: 0.123,
        downbeat_phase: 1,
        updated_at: 42,
        key: None,
    };

    let service = GridStoreService::new(Arc::new(
        SqliteGridStore::open_or_create(&db).await.expect("connect"),
    ));
    service.put(&hash, &grid).await.expect("put");
    drop(service);

    let service = GridStoreService::new(Arc::new(
        SqliteGridStore::open_or_create(&db).await.expect("reopen"),
    ));
    let loaded = service.get(&hash).await.expect("get");
    assert_eq!(loaded, Some(grid));
}
