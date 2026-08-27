//! SQLite-backed [`LibraryStore`] over the shared `library.sqlite` pool.
//!
//! The store takes the pool from [`crate::store::sqlite::SqliteGridStore`]'s
//! opener (same database, same migrations) so grids, playlists, and the
//! library index live in one file. Root removal deletes children before
//! the root inside one transaction — referential behavior must not depend
//! on FK pragmas.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use daow::{FromRow, Pool};
use error_stack::{Report, ResultExt as _};

use automixah_engine::timeline::types::TrackHash;

use super::{IndexedFile, LibraryEntry, LibraryRoot, LibraryStore, LibraryStoreError};
use crate::track::identity::now_unix;

/// SQLite-backed library store over the shared library pool.
#[derive(Clone)]
pub struct SqliteLibraryStore {
    pool: Pool,
}

/// Row shape for `SELECT … FROM library_roots`.
#[derive(Debug, Clone)]
struct RootRow {
    id: i64,
    path: String,
}

impl FromRow for RootRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            path: row.get("path")?,
        })
    }
}

impl From<RootRow> for LibraryRoot {
    fn from(row: RootRow) -> Self {
        Self {
            id: row.id,
            path: PathBuf::from(row.path),
        }
    }
}

/// Row shape for the library-files listing (`LEFT JOIN` against
/// `beat_grids` for analysis columns).
#[derive(Debug, Clone)]
struct FileRow {
    root_id: i64,
    rel_path: String,
    track_hash: String,
    title: String,
    artist: String,
    duration_seconds: Option<f64>,
    grid_bpm: Option<f64>,
    key_root: Option<i64>,
    key_mode: Option<i64>,
    mtime_secs: i64,
    size_bytes: i64,
}

impl FromRow for FileRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            root_id: row.get("root_id")?,
            rel_path: row.get("rel_path")?,
            track_hash: row.get("track_hash")?,
            title: row.get("title")?,
            artist: row.get("artist")?,
            duration_seconds: row.get("duration_seconds")?,
            grid_bpm: row.get("grid_bpm")?,
            key_root: row.get("key_root")?,
            key_mode: row.get("key_mode")?,
            mtime_secs: row.get("mtime_secs")?,
            size_bytes: row.get("size_bytes")?,
        })
    }
}

impl From<FileRow> for LibraryEntry {
    fn from(row: FileRow) -> Self {
        Self {
            root_id: row.root_id,
            rel_path: PathBuf::from(row.rel_path),
            hash: TrackHash(row.track_hash),
            title: row.title,
            artist: row.artist,
            duration: row.duration_seconds,
            bpm: row.grid_bpm,
            key: crate::store::sqlite::decode_key(row.key_root, row.key_mode),
            mtime_secs: row.mtime_secs,
            size_bytes: row.size_bytes,
        }
    }
}

/// Row shape for the scanner's change-detection projection.
#[derive(Debug, Clone)]
struct IndexedRow {
    root_id: i64,
    rel_path: String,
    mtime_secs: i64,
    size_bytes: i64,
}

impl FromRow for IndexedRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            root_id: row.get("root_id")?,
            rel_path: row.get("rel_path")?,
            mtime_secs: row.get("mtime_secs")?,
            size_bytes: row.get("size_bytes")?,
        })
    }
}

impl SqliteLibraryStore {
    /// Wraps the shared library pool (already migrated by the grid store's
    /// opener).
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Opens the database at `path` directly (runs migrations), for
    /// standalone use.
    ///
    /// # Errors
    ///
    /// Returns an error if the pool cannot connect or migrations fail.
    pub async fn open_or_create(path: &std::path::Path) -> Result<Self, Report<LibraryStoreError>> {
        let grid = crate::store::sqlite::SqliteGridStore::open_or_create(path)
            .await
            .change_context(LibraryStoreError)
            .attach("open shared library for library store")?;
        Ok(Self {
            pool: grid.pool().clone(),
        })
    }
}

#[async_trait]
impl LibraryStore for SqliteLibraryStore {
    async fn list_roots(&self) -> Result<Vec<LibraryRoot>, Report<LibraryStoreError>> {
        let rows: Vec<RootRow> = self
            .pool
            .query_all("SELECT id, path FROM library_roots ORDER BY path", vec![])
            .await
            .change_context(LibraryStoreError)
            .attach("list library roots")?;
        Ok(rows.into_iter().map(LibraryRoot::from).collect())
    }

    async fn list_entries(&self) -> Result<Vec<LibraryEntry>, Report<LibraryStoreError>> {
        // Read-time join, never denormalized: analysis facts live only in
        // `beat_grids`, so saved-grid edits flow straight through.
        let rows: Vec<FileRow> = self
            .pool
            .query_all(
                "SELECT f.root_id, f.rel_path, f.track_hash, f.title, f.artist, \
                 f.duration_seconds, b.grid_bpm, b.key_root, b.key_mode, \
                 f.mtime_secs, f.size_bytes \
                 FROM library_files f \
                 LEFT JOIN beat_grids b ON b.track_hash = f.track_hash \
                 ORDER BY f.root_id, f.rel_path",
                vec![],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("list library entries")?;
        Ok(rows.into_iter().map(LibraryEntry::from).collect())
    }

    async fn add_root(&self, path: &str) -> Result<LibraryRoot, Report<LibraryStoreError>> {
        if path.trim().is_empty() {
            return Err(
                Report::new(LibraryStoreError).attach("library root path must not be empty")
            );
        }
        let result = self
            .pool
            .execute(
                "INSERT INTO library_roots (path, added_at) VALUES (?, ?)",
                vec![Box::new(path.to_owned()), Box::new(now_unix())],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("add library root (duplicate path?)")?;
        Ok(LibraryRoot {
            id: result.last_insert_rowid,
            path: PathBuf::from(path),
        })
    }

    async fn remove_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>> {
        // Manual cascade: children first, then the root — behavior must not
        // depend on the FK pragma.
        let tx = self
            .pool
            .begin()
            .await
            .change_context(LibraryStoreError)
            .attach("begin remove-root transaction")?;
        tx.execute(
            "DELETE FROM library_files WHERE root_id = ?",
            vec![Box::new(root_id)],
        )
        .await
        .change_context(LibraryStoreError)
        .attach("delete root files")?;
        tx.execute(
            "DELETE FROM library_roots WHERE id = ?",
            vec![Box::new(root_id)],
        )
        .await
        .change_context(LibraryStoreError)
        .attach("delete root")?;
        tx.commit()
            .await
            .change_context(LibraryStoreError)
            .attach("commit remove-root transaction")?;
        Ok(())
    }

    async fn indexed_files(&self) -> Result<Vec<IndexedFile>, Report<LibraryStoreError>> {
        let rows: Vec<IndexedRow> = self
            .pool
            .query_all(
                "SELECT root_id, rel_path, mtime_secs, size_bytes FROM library_files \
                 ORDER BY root_id, rel_path",
                vec![],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("load indexed files")?;
        Ok(rows
            .into_iter()
            .map(|row| IndexedFile {
                root_id: row.root_id,
                rel_path: PathBuf::from(row.rel_path),
                mtime_secs: row.mtime_secs,
                size_bytes: row.size_bytes,
            })
            .collect())
    }

    async fn upsert_file(&self, file: &LibraryEntry) -> Result<(), Report<LibraryStoreError>> {
        self.pool
            .execute(
                "INSERT INTO library_files (root_id, rel_path, track_hash, title, artist, \
                 duration_seconds, mtime_secs, size_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(root_id, rel_path) DO UPDATE SET \
                 track_hash = excluded.track_hash, \
                 title = excluded.title, \
                 artist = excluded.artist, \
                 duration_seconds = excluded.duration_seconds, \
                 mtime_secs = excluded.mtime_secs, \
                 size_bytes = excluded.size_bytes",
                vec![
                    Box::new(file.root_id),
                    Box::new(rel_path_string(&file.rel_path)),
                    Box::new(file.hash.0.clone()),
                    Box::new(file.title.clone()),
                    Box::new(file.artist.clone()),
                    Box::new(file.duration),
                    Box::new(file.mtime_secs),
                    Box::new(file.size_bytes),
                ],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("upsert library file")?;
        Ok(())
    }

    async fn upsert_files(
        &self,
        batch: &[(LibraryEntry, String)],
    ) -> Result<(), Report<LibraryStoreError>> {
        let tx = self
            .pool
            .begin()
            .await
            .change_context(LibraryStoreError)
            .attach("begin upsert batch transaction")?;
        for (file, abs_path) in batch {
            tx.execute(
                "INSERT INTO library_files (root_id, rel_path, track_hash, title, artist, \
                 duration_seconds, mtime_secs, size_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(root_id, rel_path) DO UPDATE SET \
                 track_hash = excluded.track_hash, \
                 title = excluded.title, \
                 artist = excluded.artist, \
                 duration_seconds = excluded.duration_seconds, \
                 mtime_secs = excluded.mtime_secs, \
                 size_bytes = excluded.size_bytes",
                vec![
                    Box::new(file.root_id),
                    Box::new(rel_path_string(&file.rel_path)),
                    Box::new(file.hash.0.clone()),
                    Box::new(file.title.clone()),
                    Box::new(file.artist.clone()),
                    Box::new(file.duration),
                    Box::new(file.mtime_secs),
                    Box::new(file.size_bytes),
                ],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("upsert library file")?;
            tx.execute(
                "UPDATE playlist_tracks SET added_path = ? WHERE track_hash = ?",
                vec![Box::new(abs_path.clone()), Box::new(file.hash.0.clone())],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("refresh track paths for hash")?;
        }
        tx.commit()
            .await
            .change_context(LibraryStoreError)
            .attach("commit upsert batch transaction")?;
        Ok(())
    }

    async fn delete_files(&self, rows: &[IndexedFile]) -> Result<(), Report<LibraryStoreError>> {
        let tx = self
            .pool
            .begin()
            .await
            .change_context(LibraryStoreError)
            .attach("begin delete batch transaction")?;
        for row in rows {
            tx.execute(
                "DELETE FROM library_files WHERE root_id = ? AND rel_path = ?",
                vec![
                    Box::new(row.root_id),
                    Box::new(rel_path_string(&row.rel_path)),
                ],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("delete library file")?;
        }
        tx.commit()
            .await
            .change_context(LibraryStoreError)
            .attach("commit delete batch transaction")?;
        Ok(())
    }

    async fn delete_files_for_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>> {
        self.pool
            .execute(
                "DELETE FROM library_files WHERE root_id = ?",
                vec![Box::new(root_id)],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("delete library files for root")?;
        Ok(())
    }

    async fn refresh_track_path(
        &self,
        hash: &TrackHash,
        path: &str,
    ) -> Result<bool, Report<LibraryStoreError>> {
        let result = self
            .pool
            .execute(
                "UPDATE playlist_tracks SET added_path = ? WHERE track_hash = ?",
                vec![Box::new(path.to_owned()), Box::new(hash.0.clone())],
            )
            .await
            .change_context(LibraryStoreError)
            .attach("refresh track paths for hash")?;
        Ok(result.rows_affected > 0)
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}

/// `rel_path` as a SQLite TEXT value.
fn rel_path_string(rel_path: &Path) -> String {
    rel_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> (SqliteLibraryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteLibraryStore::open_or_create(&dir.path().join("lib.sqlite"))
            .await
            .expect("open store");
        (store, dir)
    }

    fn entry(root_id: i64, rel: &str, hash: &str) -> LibraryEntry {
        LibraryEntry {
            root_id,
            rel_path: PathBuf::from(rel),
            hash: TrackHash(hash.to_owned()),
            title: format!("Title {hash}"),
            artist: "Artist".to_owned(),
            duration: Some(61.0),
            // Scanner-produced shape: analysis columns are joins, never
            // written.
            bpm: None,
            key: None,
            mtime_secs: 100,
            size_bytes: 2048,
        }
    }

    // Given an empty store.
    // When a root is added and files are upserted.
    // Then roots and entries round-trip.
    #[tokio::test]
    async fn add_root_and_upsert_round_trip() {
        let (store, _dir) = test_store().await;
        let root = store.add_root("/music").await.expect("add root");
        store
            .upsert_file(&entry(root.id, "a/one.flac", "h1"))
            .await
            .expect("upsert");

        let roots = store.list_roots().await.expect("roots");
        assert_eq!(roots, vec![root.clone()]);
        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash.0, "h1");
        assert_eq!(entries[0].rel_path, PathBuf::from("a/one.flac"));
    }

    // Given a root.
    // When the same path is added again.
    // Then the UNIQUE constraint rejects it.
    #[tokio::test]
    async fn duplicate_root_path_is_rejected() {
        let (store, _dir) = test_store().await;
        store.add_root("/music").await.expect("first");
        let dup = store.add_root("/music").await;
        assert!(dup.is_err(), "duplicate root must be rejected");
    }

    // Given an indexed file.
    // When upserted again with new facts.
    // Then the row refreshes instead of duplicating.
    #[tokio::test]
    async fn upsert_refreshes_existing_row() {
        let (store, _dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("first");
        let mut changed = entry(root.id, "one.flac", "h2");
        changed.mtime_secs = 200;
        store.upsert_file(&changed).await.expect("second");

        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1, "one row per (root, rel_path)");
        assert_eq!(entries[0].hash.0, "h2");
        assert_eq!(entries[0].mtime_secs, 200);
    }

    // Given an indexed file.
    // When deleted as a one-element batch.
    // Then only that entry disappears.
    #[tokio::test]
    async fn delete_files_removes_one_entry() {
        let (store, _dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("one");
        store
            .upsert_file(&entry(root.id, "two.flac", "h2"))
            .await
            .expect("two");

        let rows = [IndexedFile {
            root_id: root.id,
            rel_path: PathBuf::from("one.flac"),
            mtime_secs: 100,
            size_bytes: 2048,
        }];
        store.delete_files(&rows).await.expect("delete");

        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash.0, "h2");
    }

    // Given several indexed files.
    // When deleted as one batch.
    // Then all of them disappear in the one call.
    #[tokio::test]
    async fn delete_files_removes_the_whole_batch() {
        let (store, _dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("one");
        store
            .upsert_file(&entry(root.id, "two.flac", "h2"))
            .await
            .expect("two");
        store
            .upsert_file(&entry(root.id, "three.flac", "h3"))
            .await
            .expect("three");

        let rows: Vec<IndexedFile> = ["one.flac", "three.flac"]
            .iter()
            .map(|rel| IndexedFile {
                root_id: root.id,
                rel_path: PathBuf::from(*rel),
                mtime_secs: 100,
                size_bytes: 2048,
            })
            .collect();
        store.delete_files(&rows).await.expect("delete");

        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash.0, "h2");
    }

    // Given a batch of scanned files with their disk paths.
    // When upserted as one batch.
    // Then every row lands and the paired path refreshes run in the same
    // transaction.
    #[tokio::test]
    async fn upsert_files_batches_rows_and_path_refreshes() {
        use crate::playlist::store::PlaylistStore as _;

        let (store, _dir) = test_store().await;
        let playlist = crate::playlist::store::sqlite::SqlitePlaylistStore::new(store.pool.clone());
        let list = playlist.create_playlist("mix").await.expect("playlist");
        let hash = TrackHash("h1".to_owned());
        playlist
            .insert_track(list.id, &hash, "/old/one.flac", "T", "A", None)
            .await
            .expect("insert");

        let root = store.add_root("/music").await.expect("root");
        let batch = vec![(
            entry(root.id, "one.flac", "h1"),
            "/music/one.flac".to_owned(),
        )];
        store.upsert_files(&batch).await.expect("batch");

        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash.0, "h1");
        let tracks = playlist.tracks_for(list.id).await.expect("tracks");
        assert_eq!(tracks[0].added_path, "/music/one.flac", "path refreshed");
    }

    // Given two roots with files.
    // When one root is removed.
    // Then its root row and all its files disappear; the other root's
    // files survive.
    #[tokio::test]
    async fn remove_root_cascades_to_its_files() {
        let (store, _dir) = test_store().await;
        let a = store.add_root("/a").await.expect("root a");
        let b = store.add_root("/b").await.expect("root b");
        store
            .upsert_file(&entry(a.id, "one.flac", "h1"))
            .await
            .expect("a one");
        store
            .upsert_file(&entry(b.id, "one.flac", "h1"))
            .await
            .expect("b one");
        store
            .upsert_file(&entry(b.id, "two.flac", "h2"))
            .await
            .expect("b two");

        store.remove_root(a.id).await.expect("remove");

        let roots = store.list_roots().await.expect("roots");
        assert_eq!(roots.len(), 1, "only root b remains");
        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 2, "root b's files survive");
        assert!(entries.iter().all(|e| e.root_id == b.id));
    }

    // Given an indexed file and a playlist row referencing its hash with
    // an add-time path.
    // When the track path is refreshed.
    // Then every referencing row's path updates and true is returned.
    #[tokio::test]
    async fn refresh_track_path_updates_playlist_rows() {
        use crate::playlist::store::PlaylistStore as _;

        let (store, _dir) = test_store().await;
        let playlist = crate::playlist::store::sqlite::SqlitePlaylistStore::new(store.pool.clone());
        let list = playlist.create_playlist("mix").await.expect("playlist");
        let hash = TrackHash("h1".to_owned());
        playlist
            .insert_track(list.id, &hash, "/old/one.flac", "T", "A", None)
            .await
            .expect("insert");

        let changed = store
            .refresh_track_path(&hash, "/new/one.flac")
            .await
            .expect("refresh");

        assert!(changed, "a referencing row existed");
        let tracks = playlist.tracks_for(list.id).await.expect("tracks");
        assert_eq!(tracks[0].added_path, "/new/one.flac");
    }

    // Given no playlist row for a hash.
    // When the track path is refreshed.
    // Then nothing changes and false is returned.
    #[tokio::test]
    async fn refresh_track_path_unknown_hash_is_false() {
        let (store, _dir) = test_store().await;
        let changed = store
            .refresh_track_path(&TrackHash("nobody".to_owned()), "/x")
            .await
            .expect("refresh");
        assert!(!changed);
    }

    // Given indexed files.
    // When the indexed-files projection is loaded.
    // Then change-detection facts come back keyed by (root, rel_path).
    #[tokio::test]
    async fn indexed_files_project_change_facts() {
        let (store, _dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("one");

        let indexed = store.indexed_files().await.expect("indexed");
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].rel_path, PathBuf::from("one.flac"));
        assert_eq!(indexed[0].mtime_secs, 100);
        assert_eq!(indexed[0].size_bytes, 2048);
    }

    async fn seed_grid(
        dir: &tempfile::TempDir,
        hash: &str,
        bpm: f32,
        key: Option<djcore::key::Key>,
    ) {
        use crate::store::GridStore as _;

        let grids =
            crate::store::sqlite::SqliteGridStore::open_or_create(&dir.path().join("lib.sqlite"))
                .await
                .expect("grid store");
        grids
            .put(
                &TrackHash(hash.to_owned()),
                &crate::store::GridOverride {
                    grid_bpm: bpm,
                    anchor_seconds: 0.0,
                    downbeat_phase: 0,
                    updated_at: 100,
                    key,
                },
            )
            .await
            .expect("put grid");
    }

    // Given an indexed file whose hash has a saved analysis grid.
    // When entries are listed.
    // Then the entry carries the grid's BPM and key.
    #[tokio::test]
    async fn list_entries_carries_saved_analysis() {
        let (store, dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("one");
        seed_grid(
            &dir,
            "h1",
            174.0,
            Some(djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            }),
        )
        .await;

        let entries = store.list_entries().await.expect("entries");

        assert_eq!(entries[0].bpm, Some(174.0));
        assert_eq!(
            entries[0].key,
            Some(djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            })
        );
    }

    // Given an indexed file whose hash was never analyzed.
    // When entries are listed.
    // Then the analysis columns are None, not an error.
    #[tokio::test]
    async fn list_entries_leaves_unanalyzed_columns_none() {
        let (store, _dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("one");

        let entries = store.list_entries().await.expect("entries");

        assert_eq!(entries[0].bpm, None);
        assert_eq!(entries[0].key, None);
    }

    // Given an indexed file whose hash has a saved grid.
    // When the scanner refreshes the file row.
    // Then the entry still carries the joined analysis facts.
    #[tokio::test]
    async fn rescan_does_not_disturb_joined_analysis() {
        let (store, dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("first");
        seed_grid(&dir, "h1", 128.0, None).await;

        let mut refreshed = entry(root.id, "one.flac", "h1");
        refreshed.mtime_secs = 200;
        store.upsert_file(&refreshed).await.expect("rescan");

        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mtime_secs, 200);
        assert_eq!(entries[0].bpm, Some(128.0), "join survives the rescan");
    }

    // Given one content hash indexed under two paths.
    // When entries are listed.
    // Then each file row appears once, each carrying the joined grid.
    #[tokio::test]
    async fn duplicate_hash_under_two_paths_lists_both_rows_once() {
        let (store, dir) = test_store().await;
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "a/one.flac", "h1"))
            .await
            .expect("a");
        store
            .upsert_file(&entry(root.id, "b/one.flac", "h1"))
            .await
            .expect("b");
        seed_grid(&dir, "h1", 140.0, None).await;

        let entries = store.list_entries().await.expect("entries");

        assert_eq!(entries.len(), 2, "no join multiplication");
        assert!(entries.iter().all(|e| e.bpm == Some(140.0)));
    }
}
