//! Library index persistence: the [`LibraryStore`] trait plus its service
//! wrapper.
//!
//! The library is a scanned index of the user's music directories: roots
//! (`library_roots`) and the audio files found under them
//! (`library_files`), persisted to the shared `library.sqlite`. Entries
//! are keyed by `(root_id, rel_path)` and carry the content hash plus
//! tags/duration resolved at scan time — adding an indexed track to a
//! playlist is a pure store write, no file read. The same content hash
//! legitimately appears under multiple paths; dedupe happens downstream
//! (playlist membership is checked by hash).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use error_stack::Report;
use wherror::Error;

use automixah_engine::timeline::types::TrackHash;
use djcore::key::Key;

pub mod in_memory;
pub mod sqlite;

/// Error type for library-store failures.
///
/// Carries no variants — the failure detail lives in the `error_stack::Report`
/// context attachments.
#[derive(Debug, Error)]
#[error(debug)]
pub struct LibraryStoreError;

/// A scanned root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRoot {
    /// Database id.
    pub id: i64,
    /// Canonical absolute path of the root directory.
    pub path: PathBuf,
}

/// One indexed audio file under a root.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryEntry {
    /// Owning root's database id.
    pub root_id: i64,
    /// File path relative to the root.
    pub rel_path: PathBuf,
    /// Content hash (SHA-256 hex of file bytes) — the track identity.
    pub hash: TrackHash,
    /// Tag title (or filename fallback).
    pub title: String,
    /// Tag artist (empty when unknown).
    pub artist: String,
    /// Container-probed duration in seconds, when known.
    pub duration: Option<f64>,
    /// Joined analysis BPM, read at query time from the grid library;
    /// `None` when the track has no saved beat grid.
    ///
    /// Not a scan fact and not persisted in `library_files` — always
    /// reflects the current state of `beat_grids`.
    pub bpm: Option<f64>,
    /// Joined analysis key, read at query time from the grid library;
    /// `None` when unknown.
    ///
    /// Same join semantics as [`LibraryEntry::bpm`].
    pub key: Option<Key>,
    /// File mtime at scan time (unix seconds) — the incremental-scan
    /// unchanged check.
    pub mtime_secs: i64,
    /// File size at scan time (bytes) — the incremental-scan unchanged
    /// check.
    pub size_bytes: i64,
}

/// The scan-side view of one indexed file: just the identity and
/// change-detection facts the scanner compares against the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    /// Owning root's database id.
    pub root_id: i64,
    /// File path relative to the root.
    pub rel_path: PathBuf,
    /// Indexed mtime (unix seconds).
    pub mtime_secs: i64,
    /// Indexed size (bytes).
    pub size_bytes: i64,
}

impl From<&LibraryEntry> for IndexedFile {
    fn from(entry: &LibraryEntry) -> Self {
        Self {
            root_id: entry.root_id,
            rel_path: entry.rel_path.clone(),
            mtime_secs: entry.mtime_secs,
            size_bytes: entry.size_bytes,
        }
    }
}

/// Persistence backend for the library index.
///
/// Implementations: [`sqlite::SqliteLibraryStore`] (production, daow pool
/// over `library.sqlite`) and [`in_memory::InMemoryLibraryStore`] (tests).
#[async_trait]
pub trait LibraryStore: Send + Sync {
    /// Lists all roots.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn list_roots(&self) -> Result<Vec<LibraryRoot>, Report<LibraryStoreError>>;

    /// Lists all indexed files, ordered by `(root_id, rel_path)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn list_entries(&self) -> Result<Vec<LibraryEntry>, Report<LibraryStoreError>>;

    /// Creates a root for the given (unique) path.
    ///
    /// # Errors
    ///
    /// Returns an error if the path collides with an existing root or the
    /// write fails.
    async fn add_root(&self, path: &str) -> Result<LibraryRoot, Report<LibraryStoreError>>;

    /// Removes the root and all of its indexed files (store-side prune).
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    async fn remove_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>>;

    /// All indexed files as change-detection rows for the scanner.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    async fn indexed_files(&self) -> Result<Vec<IndexedFile>, Report<LibraryStoreError>>;

    /// Upserts one scanned file: inserts when new, refreshes
    /// hash/tags/duration/mtime/size when the `(root_id, rel_path)` row
    /// already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    async fn upsert_file(&self, file: &LibraryEntry) -> Result<(), Report<LibraryStoreError>>;

    /// Upserts a batch of scanned files and refreshes the add-time
    /// playlist paths of their hashes in one transaction, so a scan
    /// commits in bounded batches instead of per-file transactions.
    ///
    /// `moves` pairs each entry with the absolute path found on disk
    /// (the value `refresh_track_path` would write) — the scanner
    /// batches both writes together because they belong to one commit.
    ///
    /// # Errors
    ///
    /// Returns an error if any statement fails; the whole batch rolls
    /// back.
    async fn upsert_files(
        &self,
        batch: &[(LibraryEntry, String)],
    ) -> Result<(), Report<LibraryStoreError>>;

    /// Deletes a batch of indexed files (vanished on disk or moved to
    /// another root) in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails; the whole batch rolls back.
    async fn delete_files(&self, rows: &[IndexedFile]) -> Result<(), Report<LibraryStoreError>>;

    /// Deletes all indexed files for `root_id` (prune on root removal).
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    async fn delete_files_for_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>>;

    /// Refreshes the add-time path of every playlist row referencing
    /// `hash` to `path` (a rescan found the content at a new location).
    /// Returns whether any row changed.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    async fn refresh_track_path(
        &self,
        hash: &TrackHash,
        path: &str,
    ) -> Result<bool, Report<LibraryStoreError>>;

    /// Backend name for debugging.
    fn name(&self) -> &'static str;
}

/// Cheap-clone service wrapper around a [`LibraryStore`] backend.
///
/// The `Services` container and the eframe app hold this, never the raw
/// trait object.
#[derive(Clone)]
pub struct LibraryStoreService {
    backend: Arc<dyn LibraryStore>,
}

impl std::fmt::Debug for LibraryStoreService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LibraryStoreService<{}>", self.backend.name())
    }
}

impl LibraryStoreService {
    /// Wraps a backend.
    #[must_use]
    pub fn new(backend: Arc<dyn LibraryStore>) -> Self {
        Self { backend }
    }

    /// Lists all roots.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    pub async fn list_roots(&self) -> Result<Vec<LibraryRoot>, Report<LibraryStoreError>> {
        self.backend.list_roots().await
    }

    /// Lists all indexed files.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    pub async fn list_entries(&self) -> Result<Vec<LibraryEntry>, Report<LibraryStoreError>> {
        self.backend.list_entries().await
    }

    /// Creates a root.
    ///
    /// # Errors
    ///
    /// Returns an error if the path collides or the write fails.
    pub async fn add_root(&self, path: &str) -> Result<LibraryRoot, Report<LibraryStoreError>> {
        self.backend.add_root(path).await
    }

    /// Removes a root and its files.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn remove_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>> {
        self.backend.remove_root(root_id).await
    }

    /// All indexed files as change-detection rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend query fails.
    pub async fn indexed_files(&self) -> Result<Vec<IndexedFile>, Report<LibraryStoreError>> {
        self.backend.indexed_files().await
    }

    /// Upserts one scanned file.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn upsert_file(&self, file: &LibraryEntry) -> Result<(), Report<LibraryStoreError>> {
        self.backend.upsert_file(file).await
    }

    /// Upserts a batch of scanned files + path refreshes in one
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if any statement fails; the batch rolls back.
    pub async fn upsert_files(
        &self,
        batch: &[(LibraryEntry, String)],
    ) -> Result<(), Report<LibraryStoreError>> {
        self.backend.upsert_files(batch).await
    }

    /// Deletes a batch of indexed files in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails; the batch rolls back.
    pub async fn delete_files(
        &self,
        rows: &[IndexedFile],
    ) -> Result<(), Report<LibraryStoreError>> {
        self.backend.delete_files(rows).await
    }

    /// Deletes all indexed files for `root_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn delete_files_for_root(
        &self,
        root_id: i64,
    ) -> Result<(), Report<LibraryStoreError>> {
        self.backend.delete_files_for_root(root_id).await
    }

    /// Refreshes playlist add-time paths for `hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn refresh_track_path(
        &self,
        hash: &TrackHash,
        path: &str,
    ) -> Result<bool, Report<LibraryStoreError>> {
        self.backend.refresh_track_path(hash, path).await
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

    // Given two roots with the same id but different paths.
    // When compared.
    // Then they are unequal (path participates in identity).
    #[test]
    fn library_root_compares_by_path() {
        let a = LibraryRoot {
            id: 1,
            path: PathBuf::from("/a"),
        };
        let b = LibraryRoot {
            id: 1,
            path: PathBuf::from("/b"),
        };
        assert_ne!(a, b);
    }

    // Given a library entry.
    // When converted to an indexed file.
    // Then only the change-detection facts carry over.
    #[test]
    fn indexed_file_projection_drops_metadata() {
        let entry = LibraryEntry {
            root_id: 1,
            rel_path: PathBuf::from("a/one.flac"),
            hash: TrackHash("h1".to_owned()),
            title: "One".to_owned(),
            artist: "Artist".to_owned(),
            duration: Some(61.0),
            bpm: Some(174.0),
            key: Some(Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            }),
            mtime_secs: 100,
            size_bytes: 2048,
        };

        let indexed = IndexedFile::from(&entry);

        assert_eq!(indexed.root_id, 1);
        assert_eq!(indexed.rel_path, PathBuf::from("a/one.flac"));
        assert_eq!(indexed.mtime_secs, 100);
        assert_eq!(indexed.size_bytes, 2048);
    }
}
