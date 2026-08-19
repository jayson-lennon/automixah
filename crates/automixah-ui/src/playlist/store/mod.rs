//! Playlist persistence: the [`PlaylistStore`] trait plus its service wrapper.
//!
//! Playlists persist to the shared `library.sqlite` as ordered content-hash
//! references (v3 schema). Track tags (artist/title/duration) persist in the
//! hash-keyed `tracks` table; the grid/key lives in `beat_grids` and joins in
//! through the same hash. Referential ordering (tracks row before
//! playlist_tracks row) is enforced here in store code, not FK cascades —
//! behavior stays identical across backends.

use std::sync::Arc;

use async_trait::async_trait;
use error_stack::Report;
use wherror::Error;

use automixah_engine::timeline::types::TrackHash;

use crate::store::GridOverride;

pub mod in_memory;
pub mod sqlite;

/// Error type for playlist-store failures.
///
/// Carries no variants — the failure detail lives in the `error_stack::Report`
/// context attachments.
#[derive(Debug, Error)]
#[error("playlist store error")]
pub struct PlaylistStoreError;

/// A playlist row in the `playlists` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSummary {
    /// Database id.
    pub id: i64,
    /// User-visible name (unique).
    pub name: String,
}

/// One playlist entry, joined across `playlist_tracks` + `tracks` +
/// `beat_grids`.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedTrack {
    /// Database id of the `playlist_tracks` row (stable identity for
    /// UI rows and worker events).
    pub id: i64,
    /// Position within the playlist (0-based, gapless).
    pub position: i64,
    /// Content hash — the identity of the track.
    pub track_hash: TrackHash,
    /// Path recorded when the track was added (display + reload hint).
    pub added_path: String,
    /// Tag title (or filename fallback).
    pub title: String,
    /// Tag artist (empty when unknown).
    pub artist: String,
    /// Duration in seconds; `None` until analyzed.
    pub duration: Option<f64>,
    /// Stored grid override; carries the detected key when present.
    pub grid: Option<GridOverride>,
}

/// Persistence backend for playlists and playlist-track metadata.
///
/// Implementations: [`sqlite::SqlitePlaylistStore`] (production, daow pool
/// over `library.sqlite`) and [`in_memory::InMemoryPlaylistStore`] (tests).
#[async_trait]
pub trait PlaylistStore: Send + Sync {
    /// Lists all playlists ordered by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn list_playlists(&self) -> Result<Vec<PlaylistSummary>, Report<PlaylistStoreError>>;

    /// Creates a playlist with the given (non-empty, unique) name.
    ///
    /// # Errors
    ///
    /// Returns an error if the name collides or the write fails.
    async fn create_playlist(
        &self,
        name: &str,
    ) -> Result<PlaylistSummary, Report<PlaylistStoreError>>;

    /// Renames a playlist.
    ///
    /// # Errors
    ///
    /// Returns an error if the target name collides or the write fails.
    async fn rename_playlist(&self, id: i64, name: &str) -> Result<(), Report<PlaylistStoreError>>;

    /// Deletes a playlist and its entries.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    async fn delete_playlist(&self, id: i64) -> Result<(), Report<PlaylistStoreError>>;

    /// Returns the playlist's tracks in position order, tags joined.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn tracks_for(&self, id: i64) -> Result<Vec<PersistedTrack>, Report<PlaylistStoreError>>;

    /// Upserts the tag row for `hash` (creating it if absent) and, when the
    /// track is new, appends it to the playlist at the next position.
    /// Returns the `playlist_tracks` rowid — the stable id the UI and
    /// worker address events by.
    ///
    /// This is the add path: a hash already present in this playlist is a
    /// duplicate and errors. The queue's re-analysis path uses
    /// [`PlaylistStore::ensure_track`] instead.
    /// `duration` of `None` leaves a stored duration untouched.
    ///
    /// # Errors
    ///
    /// Returns an error if the hash already exists in this playlist or the
    /// write fails.
    async fn insert_track(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
        path: &str,
        title: &str,
        artist: &str,
        duration: Option<f64>,
    ) -> Result<i64, Report<PlaylistStoreError>>;

    /// Idempotent variant of [`PlaylistStore::insert_track`] for rows that
    /// may already be persisted (re-enqueued analysis): upserts tags, then
    /// inserts the playlist entry with `ON CONFLICT DO NOTHING`, returning
    /// the entry's rowid either way.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    async fn ensure_track(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
        path: &str,
        title: &str,
        artist: &str,
        duration: Option<f64>,
    ) -> Result<i64, Report<PlaylistStoreError>>;

    /// Whether `hash` is already in `playlist_id` (add-path duplicate
    /// pre-check).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn contains_hash(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
    ) -> Result<bool, Report<PlaylistStoreError>>;

    /// Stored duration for `hash` (library-hit fast path).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn track_duration(
        &self,
        hash: &TrackHash,
    ) -> Result<Option<f64>, Report<PlaylistStoreError>>;

    /// Updates tag metadata for an already-inserted track (analysis done:
    /// duration, or better tags).
    ///
    /// # Errors
    ///
    /// Returns an error if the `tracks` row is missing or the write fails.
    async fn update_track_meta(
        &self,
        hash: &TrackHash,
        duration: Option<f64>,
    ) -> Result<(), Report<PlaylistStoreError>>;

    /// Removes the entry at `position` and closes the gap.
    ///
    /// # Errors
    ///
    /// Returns an error if no entry sits at `position` or the write fails.
    async fn remove_track(
        &self,
        playlist_id: i64,
        position: i64,
    ) -> Result<(), Report<PlaylistStoreError>>;

    /// Rewrites the playlist's ordering: rows are deleted and re-inserted in
    /// the given order inside one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the set of hashes differs from the stored set or
    /// the write fails.
    async fn reorder(
        &self,
        playlist_id: i64,
        ordered: &[TrackHash],
    ) -> Result<(), Report<PlaylistStoreError>>;

    /// Backend name for debugging.
    fn name(&self) -> &'static str;
}

/// Cheap-clone service wrapper around a [`PlaylistStore`] backend.
///
/// The `Services` container and the eframe app hold this, never the raw
/// trait object.
#[derive(Clone)]
pub struct PlaylistStoreService {
    backend: Arc<dyn PlaylistStore>,
}

impl std::fmt::Debug for PlaylistStoreService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlaylistStoreService<{}>", self.backend.name())
    }
}

impl PlaylistStoreService {
    /// Wraps a backend.
    #[must_use]
    pub fn new(backend: Arc<dyn PlaylistStore>) -> Self {
        Self { backend }
    }

    /// Lists all playlists ordered by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    pub async fn list_playlists(&self) -> Result<Vec<PlaylistSummary>, Report<PlaylistStoreError>> {
        self.backend.list_playlists().await
    }

    /// Creates a playlist with the given name.
    ///
    /// # Errors
    ///
    /// Returns an error if the name collides or the write fails.
    pub async fn create_playlist(
        &self,
        name: &str,
    ) -> Result<PlaylistSummary, Report<PlaylistStoreError>> {
        self.backend.create_playlist(name).await
    }

    /// Renames a playlist.
    ///
    /// # Errors
    ///
    /// Returns an error if the target name collides or the write fails.
    pub async fn rename_playlist(
        &self,
        id: i64,
        name: &str,
    ) -> Result<(), Report<PlaylistStoreError>> {
        self.backend.rename_playlist(id, name).await
    }

    /// Deletes a playlist and its entries.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn delete_playlist(&self, id: i64) -> Result<(), Report<PlaylistStoreError>> {
        self.backend.delete_playlist(id).await
    }

    /// Returns the playlist's tracks in position order, tags joined.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn tracks_for(
        &self,
        id: i64,
    ) -> Result<Vec<PersistedTrack>, Report<PlaylistStoreError>> {
        self.backend.tracks_for(id).await
    }

    /// Inserts a track (tags first, then the playlist entry), returning
    /// the entry's rowid.
    ///
    /// # Errors
    ///
    /// Returns an error if the hash already exists in this playlist or the
    /// write fails.
    pub async fn insert_track(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
        path: &str,
        title: &str,
        artist: &str,
        duration: Option<f64>,
    ) -> Result<i64, Report<PlaylistStoreError>> {
        self.backend
            .insert_track(playlist_id, hash, path, title, artist, duration)
            .await
    }

    /// Idempotent insert for re-analysis paths; returns the entry rowid.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn ensure_track(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
        path: &str,
        title: &str,
        artist: &str,
        duration: Option<f64>,
    ) -> Result<i64, Report<PlaylistStoreError>> {
        self.backend
            .ensure_track(playlist_id, hash, path, title, artist, duration)
            .await
    }

    /// Whether `hash` is already in `playlist_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn contains_hash(
        &self,
        playlist_id: i64,
        hash: &TrackHash,
    ) -> Result<bool, Report<PlaylistStoreError>> {
        self.backend.contains_hash(playlist_id, hash).await
    }

    /// Stored duration for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn track_duration(
        &self,
        hash: &TrackHash,
    ) -> Result<Option<f64>, Report<PlaylistStoreError>> {
        self.backend.track_duration(hash).await
    }

    /// Updates tag metadata for an inserted track.
    ///
    /// # Errors
    ///
    /// Returns an error if the `tracks` row is missing or the write fails.
    pub async fn update_track_meta(
        &self,
        hash: &TrackHash,
        duration: Option<f64>,
    ) -> Result<(), Report<PlaylistStoreError>> {
        self.backend.update_track_meta(hash, duration).await
    }

    /// Removes the entry at `position` and closes the gap.
    ///
    /// # Errors
    ///
    /// Returns an error if no entry sits at `position` or the write fails.
    pub async fn remove_track(
        &self,
        playlist_id: i64,
        position: i64,
    ) -> Result<(), Report<PlaylistStoreError>> {
        self.backend.remove_track(playlist_id, position).await
    }

    /// Rewrites the playlist's ordering.
    ///
    /// # Errors
    ///
    /// Returns an error if the hash set differs or the write fails.
    pub async fn reorder(
        &self,
        playlist_id: i64,
        ordered: &[TrackHash],
    ) -> Result<(), Report<PlaylistStoreError>> {
        self.backend.reorder(playlist_id, ordered).await
    }

    /// Backend name for debugging.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.backend.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given two playlist summaries with the same id but different names.
    // When compared.
    // Then they are unequal (name participates in identity).
    #[test]
    fn playlist_summary_compares_by_name() {
        let a = PlaylistSummary {
            id: 1,
            name: "a".to_owned(),
        };
        let b = PlaylistSummary {
            id: 1,
            name: "b".to_owned(),
        };
        assert_ne!(a, b);
    }
}
