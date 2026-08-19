//! Store-behavior integration tests: sqlite and in-memory backends
//! behind the same `PlaylistStore` trait, parameterized.

use automixah_ui_lib::playlist::store::PlaylistStoreService;
use automixah_ui_lib::playlist::store::in_memory::InMemoryPlaylistStore;
use automixah_ui_lib::playlist::store::sqlite::SqlitePlaylistStore;

fn memory_store() -> PlaylistStoreService {
    PlaylistStoreService::new(std::sync::Arc::new(InMemoryPlaylistStore::new()))
}

fn sqlite_store() -> PlaylistStoreService {
    let runtime = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime"),
    );
    let dir = tempfile::tempdir().expect("temp");
    let db = dir.path().join("library.sqlite");
    std::mem::forget(dir); // outlives the test body
    let pool = runtime.block_on(async {
        let grid = automixah_ui_lib::store::sqlite::SqliteGridStore::open_or_create(&db)
            .await
            .expect("open");
        grid.pool().clone()
    });
    PlaylistStoreService::new(std::sync::Arc::new(SqlitePlaylistStore::new(pool)))
}

fn runtime() -> std::sync::Arc<tokio::runtime::Runtime> {
    std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime"),
    )
}

/// Backends under test: sqlite (real migrations, real SQL) and
/// in-memory (the test fake must match behavior).
#[rstest::rstest]
#[case::sqlite(sqlite_store())]
#[case::memory(memory_store())]
// Given a fresh store.
// When a playlist is created, renamed, listed.
// Then the rename round-trips.
#[rstest::rstest]
#[case::sqlite(sqlite_store())]
#[case::memory(memory_store())]
fn rename_playlist_roundtrips(#[case] store: PlaylistStoreService) {
    {
        let rt = runtime();
        rt.block_on(async {
            let created = store.create_playlist("old").await.expect("create");
            store
                .rename_playlist(created.id, "new")
                .await
                .expect("rename");
            let listed = store.list_playlists().await.expect("list");
            let found = listed.iter().find(|p| p.id == created.id).expect("present");
            assert_eq!(found.name, "new");
        });
    }
}

// Given a playlist holding a track.
// When the same content hash is ensured again.
// Then ensure_track is idempotent (no error, no duplicate row).
#[rstest::rstest]
#[case::sqlite(sqlite_store())]
#[case::memory(memory_store())]
fn ensure_track_is_idempotent(#[case] store: PlaylistStoreService) {
    {
        let rt = runtime();
        rt.block_on(async {
            let playlist = store.create_playlist("p").await.expect("create");
            let hash = automixah_engine::timeline::types::TrackHash("h".to_owned());
            let first = store
                .ensure_track(playlist.id, &hash, "/a", "T", "A", None)
                .await
                .expect("first insert");
            let second = store
                .ensure_track(playlist.id, &hash, "/a", "T", "A", None)
                .await
                .expect("second insert is a no-op, not an error");
            assert_eq!(first, second, "same rowid both times");
            let rows = store.tracks_for(playlist.id).await.expect("rows");
            assert_eq!(rows.len(), 1, "no duplicate row");
        });
    }
}

// Given a playlist holding a track.
// When contains_hash checks that hash and an unknown one.
// Then it reports membership exactly.
#[rstest::rstest]
#[case::sqlite(sqlite_store())]
#[case::memory(memory_store())]
fn contains_hash_reports_membership(#[case] store: PlaylistStoreService) {
    {
        let rt = runtime();
        rt.block_on(async {
            let playlist = store.create_playlist("p").await.expect("create");
            let hash = automixah_engine::timeline::types::TrackHash("h".to_owned());
            store
                .ensure_track(playlist.id, &hash, "/a", "T", "A", Some(10.0))
                .await
                .expect("insert");
            assert!(
                store
                    .contains_hash(playlist.id, &hash)
                    .await
                    .expect("check"),
            );
            let other = automixah_engine::timeline::types::TrackHash("other".to_owned());
            assert!(
                !store
                    .contains_hash(playlist.id, &other)
                    .await
                    .expect("check"),
            );
        });
    }
}

// Given a track with a stored duration.
// When track_duration reads it back.
// Then the value round-trips.
#[rstest::rstest]
#[case::sqlite(sqlite_store())]
#[case::memory(memory_store())]
fn track_duration_roundtrips(#[case] store: PlaylistStoreService) {
    {
        let rt = runtime();
        rt.block_on(async {
            let playlist = store.create_playlist("p").await.expect("create");
            let hash = automixah_engine::timeline::types::TrackHash("h".to_owned());
            store
                .ensure_track(playlist.id, &hash, "/a", "T", "A", Some(122.25))
                .await
                .expect("insert");
            let duration = store
                .track_duration(&hash)
                .await
                .expect("read")
                .expect("present");
            assert!((duration - 122.25).abs() < 0.001);
        });
    }
}

// Given a track with no probed duration (probe failure path).
// When inserted with None.
// Then track_duration reports None (unknown, not 0.0).
#[rstest::rstest]
#[case::sqlite(sqlite_store())]
#[case::memory(memory_store())]
fn null_duration_stays_none(#[case] store: PlaylistStoreService) {
    {
        let rt = runtime();
        rt.block_on(async {
            let playlist = store.create_playlist("p").await.expect("create");
            let hash = automixah_engine::timeline::types::TrackHash("h".to_owned());
            store
                .ensure_track(playlist.id, &hash, "/a", "T", "A", None)
                .await
                .expect("insert");
            assert!(
                store.track_duration(&hash).await.expect("read").is_none(),
                "NULL stays unknown"
            );
        });
    }
}
