//! SQLite-backed [`PlaylistStore`] over the shared `library.sqlite` pool.
//!
//! The store takes the pool from [`crate::store::sqlite::SqliteGridStore`]'s
//! opener (same database, same migrations) so grids, keys, tags, and
//! playlists live in one file. `reorder` rewrites positions inside a
//! transaction; duplicate inserts surface the schema's UNIQUE violation.

use async_trait::async_trait;
use daow::{FromRow, Pool};
use error_stack::{Report, ResultExt as _};

use automixah_engine::timeline::types::TrackHash;

use super::{PersistedTrack, PlaylistStore, PlaylistStoreError, PlaylistSummary, ReorderOutcome};
use crate::store::GridOverride;
use crate::track::identity::now_unix;

/// SQLite-backed playlist store over the shared library pool.
#[derive(Clone)]
pub struct SqlitePlaylistStore {
    pool: Pool,
}

/// Row shape for `SELECT … FROM playlists`.
#[derive(Debug, Clone)]
struct PlaylistRow {
    id: i64,
    name: String,
}

impl FromRow for PlaylistRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
        })
    }
}

impl From<PlaylistRow> for PlaylistSummary {
    fn from(row: PlaylistRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
        }
    }
}

/// Row shape for the joined playlist-tracks query.
#[derive(Debug, Clone)]
struct JoinedTrackRow {
    id: i64,
    position: i64,
    track_hash: String,
    added_path: String,
    title: String,
    artist: String,
    duration_seconds: Option<f64>,
    grid_bpm: Option<f64>,
    anchor_seconds: Option<f64>,
    downbeat_phase: Option<i64>,
    updated_at: Option<i64>,
    key_root: Option<i64>,
    key_mode: Option<i64>,
}

impl FromRow for JoinedTrackRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            position: row.get("position")?,
            track_hash: row.get("track_hash")?,
            added_path: row.get("added_path")?,
            title: row.get("title")?,
            artist: row.get("artist")?,
            duration_seconds: row.get("duration_seconds")?,
            grid_bpm: row.get("grid_bpm")?,
            anchor_seconds: row.get("anchor_seconds")?,
            downbeat_phase: row.get("downbeat_phase")?,
            updated_at: row.get("updated_at")?,
            key_root: row.get("key_root")?,
            key_mode: row.get("key_mode")?,
        })
    }
}

impl From<JoinedTrackRow> for PersistedTrack {
    fn from(row: JoinedTrackRow) -> Self {
        #[expect(clippy::cast_possible_truncation, reason = "SQLite REAL is f64")]
        let grid = row.grid_bpm.map(|grid_bpm| GridOverride {
            grid_bpm: grid_bpm as f32,
            anchor_seconds: row.anchor_seconds.unwrap_or(0.0) as f32,
            downbeat_phase: row.downbeat_phase.unwrap_or(0) as u8,
            updated_at: row.updated_at.unwrap_or(0),
            key: crate::store::sqlite::decode_key(row.key_root, row.key_mode),
        });
        Self {
            id: row.id,
            position: row.position,
            track_hash: TrackHash(row.track_hash),
            added_path: row.added_path,
            title: row.title,
            artist: row.artist,
            duration: row.duration_seconds,
            grid,
        }
    }
}

impl SqlitePlaylistStore {
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
    pub async fn open_or_create(
        path: &std::path::Path,
    ) -> Result<Self, Report<PlaylistStoreError>> {
        let grid = crate::store::sqlite::SqliteGridStore::open_or_create(path)
            .await
            .change_context(PlaylistStoreError)
            .attach("open shared library for playlist store")?;
        Ok(Self {
            pool: grid.pool().clone(),
        })
    }
}

#[async_trait]
impl PlaylistStore for SqlitePlaylistStore {
    async fn list_playlists(&self) -> Result<Vec<PlaylistSummary>, Report<PlaylistStoreError>> {
        let rows: Vec<PlaylistRow> = self
            .pool
            .query_all("SELECT id, name FROM playlists ORDER BY name", vec![])
            .await
            .change_context(PlaylistStoreError)
            .attach("list playlists")?;
        Ok(rows.into_iter().map(PlaylistSummary::from).collect())
    }

    async fn create_playlist(
        &self,
        name: &str,
    ) -> Result<PlaylistSummary, Report<PlaylistStoreError>> {
        if name.trim().is_empty() {
            return Err(Report::new(PlaylistStoreError).attach("playlist name must not be empty"));
        }
        let result = self
            .pool
            .execute(
                "INSERT INTO playlists (name, created_at) VALUES (?, ?)",
                vec![Box::new(name.to_owned()), Box::new(now_unix())],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("create playlist")?;
        Ok(PlaylistSummary {
            id: result.last_insert_rowid,
            name: name.to_owned(),
        })
    }

    async fn rename_playlist(&self, id: i64, name: &str) -> Result<(), Report<PlaylistStoreError>> {
        self.pool
            .execute(
                "UPDATE playlists SET name = ? WHERE id = ?",
                vec![Box::new(name.to_owned()), Box::new(id)],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("rename playlist")?;
        Ok(())
    }

    async fn delete_playlist(&self, id: i64) -> Result<(), Report<PlaylistStoreError>> {
        // Manual cascade: entries first, then the playlist row. FKs may be
        // enforced by the pool, but behavior must not depend on the pragma.
        let tx = self
            .pool
            .begin()
            .await
            .change_context(PlaylistStoreError)
            .attach("begin delete-playlist transaction")?;
        tx.execute(
            "DELETE FROM playlist_tracks WHERE playlist_id = ?",
            vec![Box::new(id)],
        )
        .await
        .change_context(PlaylistStoreError)
        .attach("delete playlist entries")?;
        tx.execute("DELETE FROM playlists WHERE id = ?", vec![Box::new(id)])
            .await
            .change_context(PlaylistStoreError)
            .attach("delete playlist")?;
        tx.commit()
            .await
            .change_context(PlaylistStoreError)
            .attach("commit delete-playlist transaction")?;
        Ok(())
    }

    async fn tracks_for(&self, id: i64) -> Result<Vec<PersistedTrack>, Report<PlaylistStoreError>> {
        let rows: Vec<JoinedTrackRow> = self
            .pool
            .query_all(
                "SELECT pt.rowid AS id, pt.position, pt.track_hash, pt.added_path, \
                 t.title, t.artist, t.duration_seconds, \
                 g.grid_bpm, g.anchor_seconds, g.downbeat_phase, g.updated_at, g.key_root, g.key_mode \
                 FROM playlist_tracks pt \
                 JOIN tracks t ON t.track_hash = pt.track_hash \
                 LEFT JOIN beat_grids g ON g.track_hash = pt.track_hash \
                 WHERE pt.playlist_id = ? \
                 ORDER BY pt.position",
                vec![Box::new(id)],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("load playlist tracks")?;
        Ok(rows.into_iter().map(PersistedTrack::from).collect())
    }

    async fn insert_track(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
        path: &str,
        title: &str,
        artist: &str,
        duration: Option<f64>,
    ) -> Result<i64, Report<PlaylistStoreError>> {
        let tx = self
            .pool
            .begin()
            .await
            .change_context(PlaylistStoreError)
            .attach("begin insert-track transaction")?;
        // Tags first: the playlist entry references the tracks row
        // (referential ordering enforced in code).
        tx.execute(
            "INSERT INTO tracks (track_hash, title, artist, duration_seconds, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(track_hash) DO UPDATE SET \
             title = excluded.title, \
             artist = excluded.artist, \
             duration_seconds = COALESCE(excluded.duration_seconds, tracks.duration_seconds), \
             updated_at = excluded.updated_at",
            vec![
                Box::new(hash.0.clone()),
                Box::new(title.to_owned()),
                Box::new(artist.to_owned()),
                Box::new(duration),
                Box::new(now_unix()),
            ],
        )
        .await
        .change_context(PlaylistStoreError)
        .attach("upsert track tags")?;
        let next: i64 = tx
            .query_one(
                "SELECT COALESCE(MAX(position) + 1, 0) AS next FROM playlist_tracks WHERE playlist_id = ?",
                vec![Box::new(playlist_id)],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("compute next position")?
            .map_or(0, |r| r);
        let insert = tx
            .execute(
                "INSERT INTO playlist_tracks (playlist_id, position, track_hash, added_path) \
                 VALUES (?, ?, ?, ?)",
                vec![
                    Box::new(playlist_id),
                    Box::new(next),
                    Box::new(hash.0.clone()),
                    Box::new(path.to_owned()),
                ],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("insert playlist entry (duplicate hash in playlist?)")?;
        let rowid = insert.last_insert_rowid;
        tx.commit()
            .await
            .change_context(PlaylistStoreError)
            .attach("commit insert-track transaction")?;
        Ok(rowid)
    }

    async fn ensure_track(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
        path: &str,
        title: &str,
        artist: &str,
        duration: Option<f64>,
    ) -> Result<i64, Report<PlaylistStoreError>> {
        let tx = self
            .pool
            .begin()
            .await
            .change_context(PlaylistStoreError)
            .attach("begin ensure-track transaction")?;
        // Same tags upsert as the add path (referential ordering).
        tx.execute(
            "INSERT INTO tracks (track_hash, title, artist, duration_seconds, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(track_hash) DO UPDATE SET \
             title = excluded.title, \
             artist = excluded.artist, \
             duration_seconds = COALESCE(excluded.duration_seconds, tracks.duration_seconds), \
             updated_at = excluded.updated_at",
            vec![
                Box::new(hash.0.clone()),
                Box::new(title.to_owned()),
                Box::new(artist.to_owned()),
                Box::new(duration),
                Box::new(now_unix()),
            ],
        )
        .await
        .change_context(PlaylistStoreError)
        .attach("upsert track tags")?;
        // Idempotent entry insert: OR IGNORE keeps an existing entry
        // (re-enqueued analysis) intact instead of tripping UNIQUE.
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO playlist_tracks (playlist_id, position, track_hash, added_path) \
                 SELECT ?, COALESCE(MAX(position) + 1, 0), ?, ? FROM playlist_tracks WHERE playlist_id = ?",
                vec![
                    Box::new(playlist_id),
                    Box::new(hash.0.clone()),
                    Box::new(path.to_owned()),
                    Box::new(playlist_id),
                ],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("ensure playlist entry")?;
        let rowid = if inserted.rows_affected > 0 {
            inserted.last_insert_rowid
        } else {
            tx.query_one(
                "SELECT rowid AS id FROM playlist_tracks WHERE playlist_id = ? AND track_hash = ?",
                vec![Box::new(playlist_id), Box::new(hash.0.clone())],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("look up existing playlist entry")?
            .ok_or_else(|| {
                Report::new(PlaylistStoreError).attach("ensure inserted no row and found none")
            })?
        };
        tx.commit()
            .await
            .change_context(PlaylistStoreError)
            .attach("commit ensure-track transaction")?;
        Ok(rowid)
    }

    async fn contains_hash(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
    ) -> Result<bool, Report<PlaylistStoreError>> {
        let present: Option<i64> = self
            .pool
            .query_one(
                "SELECT 1 FROM playlist_tracks WHERE playlist_id = ? AND track_hash = ?",
                vec![Box::new(playlist_id), Box::new(hash.0.clone())],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("check hash in playlist")?;
        Ok(present.is_some())
    }

    async fn track_duration(
        &self,
        hash: &TrackHash,
    ) -> Result<Option<f64>, Report<PlaylistStoreError>> {
        let row: Option<DurationRow> = self
            .pool
            .query_one(
                "SELECT duration_seconds FROM tracks WHERE track_hash = ?",
                vec![Box::new(hash.0.clone())],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("load track duration")?;
        Ok(row.and_then(|r| r.duration_seconds))
    }

    async fn update_track_meta(
        &self,
        hash: &TrackHash,
        duration: Option<f64>,
    ) -> Result<(), Report<PlaylistStoreError>> {
        let result = self
            .pool
            .execute(
                "UPDATE tracks SET duration_seconds = COALESCE(?, duration_seconds), updated_at = ? \
                 WHERE track_hash = ?",
                vec![
                    Box::new(duration),
                    Box::new(now_unix()),
                    Box::new(hash.0.clone()),
                ],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("update track metadata")?;
        if result.rows_affected == 0 {
            return Err(Report::new(PlaylistStoreError)
                .attach("no tracks row for hash")
                .attach(hash.0.clone()));
        }
        Ok(())
    }

    async fn remove_track(
        &self,
        playlist_id: i64,
        position: i64,
    ) -> Result<(), Report<PlaylistStoreError>> {
        let tx = self
            .pool
            .begin()
            .await
            .change_context(PlaylistStoreError)
            .attach("begin remove-track transaction")?;
        let result = tx
            .execute(
                "DELETE FROM playlist_tracks WHERE playlist_id = ? AND position = ?",
                vec![Box::new(playlist_id), Box::new(position)],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("delete playlist entry")?;
        if result.rows_affected == 0 {
            return Err(Report::new(PlaylistStoreError)
                .attach("no entry at position")
                .attach(position.to_string()));
        }
        tx.execute(
            "UPDATE playlist_tracks SET position = position - 1 \
             WHERE playlist_id = ? AND position > ?",
            vec![Box::new(playlist_id), Box::new(position)],
        )
        .await
        .change_context(PlaylistStoreError)
        .attach("close position gap")?;
        tx.commit()
            .await
            .change_context(PlaylistStoreError)
            .attach("commit remove-track transaction")?;
        Ok(())
    }

    async fn reorder(
        &self,
        playlist_id: i64,
        ordered: &[TrackHash],
    ) -> Result<ReorderOutcome, Report<PlaylistStoreError>> {
        let tx = self
            .pool
            .begin()
            .await
            .change_context(PlaylistStoreError)
            .attach("begin reorder transaction")?;
        let stored: Vec<EntryRow> = tx
            .query_all(
                "SELECT track_hash, added_path FROM playlist_tracks WHERE playlist_id = ? ORDER BY position",
                vec![Box::new(playlist_id)],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("load stored order")?;
        let original: Vec<TrackHash> = stored
            .iter()
            .map(|entry| TrackHash(entry.track_hash.clone()))
            .collect();
        let mut incoming: Vec<String> = ordered.iter().map(|h| h.0.clone()).collect();
        incoming.sort();
        let mut expected: Vec<String> = stored.iter().map(|r| r.track_hash.clone()).collect();
        expected.sort();
        let duplicate = ordered
            .iter()
            .enumerate()
            .any(|(index, hash)| ordered[..index].contains(hash));
        if incoming != expected || duplicate {
            return Ok(ReorderOutcome::Rejected {
                order: original,
                error: Report::new(PlaylistStoreError)
                    .attach("reorder hash set differs from stored set"),
            });
        }
        if let Err(error) = async {
            tx.execute(
                "DELETE FROM playlist_tracks WHERE playlist_id = ?",
                vec![Box::new(playlist_id)],
            )
            .await
            .change_context(PlaylistStoreError)
            .attach("clear playlist entries for rewrite")?;
            for (position, hash) in ordered.iter().enumerate() {
                let path = stored
                    .iter()
                    .find(|row| row.track_hash == hash.0)
                    .map_or_else(String::new, |row| row.added_path.clone());
                tx.execute(
                    "INSERT INTO playlist_tracks (playlist_id, position, track_hash, added_path) VALUES (?, ?, ?, ?)",
                    vec![
                        Box::new(playlist_id),
                        Box::new(i64::try_from(position).change_context(PlaylistStoreError)?),
                        Box::new(hash.0.clone()),
                        Box::new(path),
                    ],
                )
                .await
                .change_context(PlaylistStoreError)
                .attach("re-insert ordered entry")?;
            }
            tx.commit()
                .await
                .change_context(PlaylistStoreError)
                .attach("commit reorder transaction")?;
            Ok::<(), Report<PlaylistStoreError>>(())
        }
        .await
        {
            return Ok(ReorderOutcome::Rejected {
                order: original,
                error,
            });
        }
        Ok(ReorderOutcome::Saved {
            order: ordered.to_vec(),
        })
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}

/// Row shape for a single-column duration lookup.
#[derive(Debug, Clone)]
struct DurationRow {
    duration_seconds: Option<f64>,
}

impl FromRow for DurationRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            duration_seconds: row.get("duration_seconds")?,
        })
    }
}

/// Row shape for a playlist entry read before a reorder rewrite.
#[derive(Debug, Clone)]
struct EntryRow {
    track_hash: String,
    added_path: String,
}

impl FromRow for EntryRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            track_hash: row.get("track_hash")?,
            added_path: row.get("added_path")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> (SqlitePlaylistStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqlitePlaylistStore::open_or_create(&dir.path().join("lib.sqlite"))
            .await
            .expect("open store");
        (store, dir)
    }

    // Given an empty store.
    // When a playlist is created.
    // Then it appears in the list.
    #[tokio::test]
    async fn create_playlist_lists() {
        let (store, _dir) = test_store().await;
        store.create_playlist("mix 1").await.expect("create");

        let lists = store.list_playlists().await.expect("list");
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "mix 1");
    }

    // Given a playlist with tracks.
    // When queried.
    // Then tracks come back in position order with tags.
    #[tokio::test]
    async fn tracks_for_orders_by_position() {
        let (store, _dir) = test_store().await;
        let list = store.create_playlist("ordered").await.expect("create");
        let b = TrackHash("bb".to_owned());
        let a = TrackHash("aa".to_owned());
        store
            .insert_track(list.id, &b, "/b.wav", "B Title", "B Artist", None)
            .await
            .expect("insert b");
        store
            .insert_track(list.id, &a, "/a.wav", "A Title", "A Artist", None)
            .await
            .expect("insert a");

        let tracks = store.tracks_for(list.id).await.expect("tracks");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_hash, b, "first inserted sits first");
        assert_eq!(tracks[1].track_hash, a);
        assert_eq!(tracks[0].title, "B Title");
    }

    // Given a playlist containing a hash.
    // When the same hash is inserted again.
    // Then the UNIQUE constraint rejects it.
    #[tokio::test]
    async fn duplicate_hash_in_playlist_is_rejected() {
        let (store, _dir) = test_store().await;
        let list = store.create_playlist("dup").await.expect("create");
        let hash = TrackHash("same".to_owned());
        store
            .insert_track(list.id, &hash, "/x.wav", "T", "A", None)
            .await
            .expect("first insert");
        let dup = store
            .insert_track(list.id, &hash, "/y.wav", "T2", "A2", None)
            .await;
        assert!(dup.is_err(), "duplicate must be rejected");
    }

    // Given a playlist with two tracks.
    // When the middle entry is removed.
    // Then positions close the gap.
    #[tokio::test]
    async fn remove_track_closes_position_gap() {
        let (store, _dir) = test_store().await;
        let list = store.create_playlist("gap").await.expect("create");
        for name in ["a", "b", "c"] {
            store
                .insert_track(list.id, &TrackHash(name.to_owned()), "/x", name, "", None)
                .await
                .expect("insert");
        }
        store.remove_track(list.id, 1).await.expect("remove middle");

        let tracks = store.tracks_for(list.id).await.expect("tracks");
        let positions: Vec<i64> = tracks.iter().map(|t| t.position).collect();
        assert_eq!(positions, vec![0, 1], "gapless after removal");
        assert_eq!(tracks[1].track_hash.0, "c");
    }

    // Given a playlist with three tracks.
    // When reordered to a different permutation.
    // Then the new order persists.
    #[tokio::test]
    async fn reorder_rewrites_ordering() {
        let (store, _dir) = test_store().await;
        let list = store.create_playlist("reorder").await.expect("create");
        for name in ["a", "b", "c"] {
            store
                .insert_track(list.id, &TrackHash(name.to_owned()), "/x", name, "", None)
                .await
                .expect("insert");
        }
        store
            .reorder(
                list.id,
                &[
                    TrackHash("c".to_owned()),
                    TrackHash("a".to_owned()),
                    TrackHash("b".to_owned()),
                ],
            )
            .await
            .expect("reorder");

        let tracks = store.tracks_for(list.id).await.expect("tracks");
        let order: Vec<String> = tracks.iter().map(|t| t.track_hash.0.clone()).collect();
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    // Given a playlist with two tracks.
    // When an invalid hash set is reordered.
    // Then the rejection returns the durable order and the transaction leaves it unchanged.
    #[tokio::test]
    async fn reorder_rejects_with_rollback_order() {
        let (store, _dir) = test_store().await;
        let list = store.create_playlist("rollback").await.expect("create");
        for name in ["a", "b"] {
            store
                .insert_track(list.id, &TrackHash(name.to_owned()), "/x", name, "", None)
                .await
                .expect("insert");
        }

        let outcome = store
            .reorder(
                list.id,
                &[TrackHash("a".to_owned()), TrackHash("missing".to_owned())],
            )
            .await
            .expect("outcome");

        match outcome {
            ReorderOutcome::Rejected { order, .. } => {
                assert_eq!(
                    order,
                    vec![TrackHash("a".to_owned()), TrackHash("b".to_owned())]
                );
            }
            ReorderOutcome::Saved { .. } => panic!("invalid order saved"),
        }
        let rows = store.tracks_for(list.id).await.expect("rows");
        assert_eq!(
            rows.iter()
                .map(|row| row.track_hash.0.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
    // Given a stored track with duration.
    // When meta is updated with None.
    // Then the stored duration survives.
    #[tokio::test]
    async fn update_track_meta_preserves_duration() {
        let (store, _dir) = test_store().await;
        let list = store.create_playlist("meta").await.expect("create");
        let hash = TrackHash("dur".to_owned());
        store
            .insert_track(list.id, &hash, "/x", "T", "A", Some(120.5))
            .await
            .expect("insert");

        store.update_track_meta(&hash, None).await.expect("update");
        let tracks = store.tracks_for(list.id).await.expect("tracks");
        assert_eq!(tracks[0].duration, Some(120.5));
    }
}
