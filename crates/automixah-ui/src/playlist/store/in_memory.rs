//! In-memory [`PlaylistStore`] backend for tests and headless consumers.
//!
//! Mirrors the SQLite semantics: positions are gapless 0-based, duplicate
//! hashes within one playlist are rejected, and `reorder` rewrites positions
//! from the given hash order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

use async_trait::async_trait;
use error_stack::Report;
use parking_lot::Mutex;

use automixah_engine::timeline::types::TrackHash;

use super::{PersistedTrack, PlaylistStore, PlaylistStoreError, PlaylistSummary, ReorderOutcome};
use crate::store::GridOverride;

/// One stored playlist.
#[derive(Debug, Clone)]
struct PlaylistData {
    name: String,
    /// Entries in position order.
    entries: Vec<Entry>,
}

/// One playlist entry.
#[derive(Debug, Clone)]
struct Entry {
    /// Stable entry id (stands in for `playlist_tracks.rowid`).
    id: i64,
    hash: TrackHash,
    path: String,
}

/// Tags for a track hash (shared across playlists).
#[derive(Debug, Clone)]
struct TrackMeta {
    title: String,
    artist: String,
    duration: Option<f64>,
    /// Joined grid override when the hash was analyzed (test seam
    /// standing in for the grid library).
    grid: Option<GridOverride>,
}

/// HashMap-backed playlist store.
#[derive(Debug, Default)]
pub struct InMemoryPlaylistStore {
    next_id: AtomicI64,
    playlists: Mutex<HashMap<i64, PlaylistData>>,
    tracks: Mutex<HashMap<String, TrackMeta>>,
    /// Entry-rowid mint: entries get ids 1, 2, 3… across playlists.
    entry_ids: AtomicI64,
}

impl InMemoryPlaylistStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared tags upsert for the insert/ensure paths.
    fn upsert_tags(&self, hash: &TrackHash, title: &str, artist: &str, duration: Option<f64>) {
        let mut tracks = self.tracks.lock();
        let entry = tracks.entry(hash.0.clone()).or_insert(TrackMeta {
            title: String::new(),
            artist: String::new(),
            duration: None,
            grid: None,
        });
        entry.title = title.to_owned();
        entry.artist = artist.to_owned();
        if duration.is_some() {
            entry.duration = duration;
        }
    }

    /// Seeds a grid override for a hash (tests simulate the grid library).
    pub fn seed_grid(&self, hash: &TrackHash, grid: GridOverride) {
        let mut tracks = self.tracks.lock();
        let meta = tracks.entry(hash.0.clone()).or_insert(TrackMeta {
            title: String::new(),
            artist: String::new(),
            duration: None,
            grid: None,
        });
        meta.grid = Some(grid);
    }
}

#[async_trait]
impl PlaylistStore for InMemoryPlaylistStore {
    async fn list_playlists(&self) -> Result<Vec<PlaylistSummary>, Report<PlaylistStoreError>> {
        let playlists = self.playlists.lock();
        let mut summaries: Vec<PlaylistSummary> = playlists
            .iter()
            .map(|(id, data)| PlaylistSummary {
                id: *id,
                name: data.name.clone(),
            })
            .collect();
        summaries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(summaries)
    }

    async fn create_playlist(
        &self,
        name: &str,
    ) -> Result<PlaylistSummary, Report<PlaylistStoreError>> {
        if name.trim().is_empty() {
            return Err(Report::new(PlaylistStoreError).attach("playlist name must not be empty"));
        }
        let playlists = self.playlists.lock();
        if playlists.values().any(|p| p.name == name) {
            return Err(Report::new(PlaylistStoreError).attach("playlist name already exists"));
        }
        drop(playlists);
        let id = self.next_id.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.playlists.lock().insert(
            id,
            PlaylistData {
                name: name.to_owned(),
                entries: Vec::new(),
            },
        );
        Ok(PlaylistSummary {
            id,
            name: name.to_owned(),
        })
    }

    async fn rename_playlist(&self, id: i64, name: &str) -> Result<(), Report<PlaylistStoreError>> {
        let mut playlists = self.playlists.lock();
        let Some(data) = playlists.get_mut(&id) else {
            return Err(Report::new(PlaylistStoreError).attach("no such playlist"));
        };
        data.name = name.to_owned();
        Ok(())
    }

    async fn delete_playlist(&self, id: i64) -> Result<(), Report<PlaylistStoreError>> {
        self.playlists.lock().remove(&id);
        Ok(())
    }

    async fn tracks_for(&self, id: i64) -> Result<Vec<PersistedTrack>, Report<PlaylistStoreError>> {
        let playlists = self.playlists.lock();
        let tracks = self.tracks.lock();
        let Some(data) = playlists.get(&id) else {
            return Ok(Vec::new());
        };
        Ok(data
            .entries
            .iter()
            .enumerate()
            .map(|(position, entry)| {
                let meta = tracks.get(&entry.hash.0);
                PersistedTrack {
                    id: entry.id,
                    position: i64::try_from(position).unwrap_or(i64::MAX),
                    track_hash: entry.hash.clone(),
                    added_path: entry.path.clone(),
                    title: meta.as_ref().map_or_else(String::new, |m| m.title.clone()),
                    artist: meta.as_ref().map_or_else(String::new, |m| m.artist.clone()),
                    duration: meta.as_ref().and_then(|m| m.duration),
                    grid: meta.as_ref().and_then(|m| m.grid.clone()),
                }
            })
            .collect())
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
        // Tags first (referential ordering), then the playlist entry.
        self.upsert_tags(hash, title, artist, duration);
        let mut playlists = self.playlists.lock();
        let Some(data) = playlists.get_mut(&playlist_id) else {
            return Err(Report::new(PlaylistStoreError).attach("no such playlist"));
        };
        if data.entries.iter().any(|e| e.hash == *hash) {
            return Err(Report::new(PlaylistStoreError).attach("duplicate hash in playlist"));
        }
        let id = self.entry_ids.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        data.entries.push(Entry {
            id,
            hash: hash.clone(),
            path: path.to_owned(),
        });
        Ok(id)
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
        self.upsert_tags(hash, title, artist, duration);
        let mut playlists = self.playlists.lock();
        let Some(data) = playlists.get_mut(&playlist_id) else {
            return Err(Report::new(PlaylistStoreError).attach("no such playlist"));
        };
        if let Some(existing) = data.entries.iter().find(|e| e.hash == *hash) {
            return Ok(existing.id);
        }
        let id = self.entry_ids.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        data.entries.push(Entry {
            id,
            hash: hash.clone(),
            path: path.to_owned(),
        });
        Ok(id)
    }

    async fn contains_hash(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
    ) -> Result<bool, Report<PlaylistStoreError>> {
        let playlists = self.playlists.lock();
        Ok(playlists
            .get(&playlist_id)
            .is_some_and(|data| data.entries.iter().any(|e| e.hash == *hash)))
    }

    async fn track_duration(
        &self,
        hash: &TrackHash,
    ) -> Result<Option<f64>, Report<PlaylistStoreError>> {
        let tracks = self.tracks.lock();
        Ok(tracks.get(&hash.0).and_then(|m| m.duration))
    }

    async fn update_track_meta(
        &self,
        hash: &TrackHash,
        duration: Option<f64>,
    ) -> Result<(), Report<PlaylistStoreError>> {
        let mut tracks = self.tracks.lock();
        let Some(meta) = tracks.get_mut(&hash.0) else {
            return Err(Report::new(PlaylistStoreError).attach("no tracks row for hash"));
        };
        if duration.is_some() {
            meta.duration = duration;
        }
        Ok(())
    }

    async fn remove_track(
        &self,
        playlist_id: i64,
        position: i64,
    ) -> Result<(), Report<PlaylistStoreError>> {
        let mut playlists = self.playlists.lock();
        let Some(data) = playlists.get_mut(&playlist_id) else {
            return Err(Report::new(PlaylistStoreError).attach("no such playlist"));
        };
        let index = usize::try_from(position).unwrap_or(usize::MAX);
        if index >= data.entries.len() {
            return Err(Report::new(PlaylistStoreError).attach("no entry at position"));
        }
        data.entries.remove(index);
        Ok(())
    }

    async fn reorder(
        &self,
        playlist_id: i64,
        ordered: &[TrackHash],
    ) -> Result<ReorderOutcome, Report<PlaylistStoreError>> {
        let mut playlists = self.playlists.lock();
        let Some(data) = playlists.get_mut(&playlist_id) else {
            return Err(Report::new(PlaylistStoreError).attach("no such playlist"));
        };
        let original: Vec<TrackHash> = data
            .entries
            .iter()
            .map(|entry| entry.hash.clone())
            .collect();
        let mut expected = original.clone();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        let mut incoming = ordered.to_vec();
        incoming.sort_by(|a, b| a.0.cmp(&b.0));
        if incoming != expected
            || ordered.iter().any(|hash| {
                ordered
                    .iter()
                    .filter(|candidate| *candidate == hash)
                    .count()
                    > 1
            })
        {
            return Ok(ReorderOutcome::Rejected {
                order: original,
                error: Report::new(PlaylistStoreError)
                    .attach("reorder hash set differs from stored set"),
            });
        }
        let mut new_entries = Vec::with_capacity(ordered.len());
        for hash in ordered {
            let entry = data
                .entries
                .iter()
                .find(|entry| entry.hash == *hash)
                .expect("validated reorder hash set");
            new_entries.push(entry.clone());
        }
        data.entries = new_entries;
        Ok(ReorderOutcome::Saved {
            order: ordered.to_vec(),
        })
    }

    fn name(&self) -> &'static str {
        "in-memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given an empty in-memory store.
    // When a playlist is created and listed.
    // Then it appears with its id and name.
    #[tokio::test]
    async fn in_memory_create_and_list() {
        let store = InMemoryPlaylistStore::new();
        let created = store.create_playlist("demo").await.expect("create");
        let lists = store.list_playlists().await.expect("list");
        assert_eq!(lists, vec![created]);
    }

    // Given a playlist.
    // When a duplicate hash is inserted.
    // Then it is rejected.
    #[tokio::test]
    async fn in_memory_rejects_duplicate_hash() {
        let store = InMemoryPlaylistStore::new();
        let list = store.create_playlist("dup").await.expect("create");
        let hash = TrackHash("h".to_owned());
        store
            .insert_track(list.id, &hash, "/x", "T", "A", None)
            .await
            .expect("first");
        assert!(
            store
                .insert_track(list.id, &hash, "/y", "T2", "A2", None)
                .await
                .is_err()
        );
    }

    // Given a playlist with two tracks.
    // When an invalid hash set is reordered.
    // Then the rejection includes the original durable order and storage is unchanged.
    #[tokio::test]
    async fn in_memory_reorder_rejects_with_rollback_order() {
        let store = InMemoryPlaylistStore::new();
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
        assert_eq!(rows[1].track_hash.0, "b");
    }
    // Given two playlists.
    // When the same hash is inserted into both.
    // Then both accept it (cross-playlist duplicates allowed).
    #[tokio::test]
    async fn in_memory_same_hash_across_playlists_allowed() {
        let store = InMemoryPlaylistStore::new();
        let a = store.create_playlist("a").await.expect("create a");
        let b = store.create_playlist("b").await.expect("create b");
        let hash = TrackHash("shared".to_owned());
        store
            .insert_track(a.id, &hash, "/x", "T", "A", None)
            .await
            .expect("insert a");
        store
            .insert_track(b.id, &hash, "/x", "T", "A", None)
            .await
            .expect("insert b");
        assert_eq!(store.tracks_for(a.id).await.expect("a").len(), 1);
        assert_eq!(store.tracks_for(b.id).await.expect("b").len(), 1);
    }
}
