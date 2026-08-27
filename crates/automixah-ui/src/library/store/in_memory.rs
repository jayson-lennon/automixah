//! In-memory [`LibraryStore`] for tests.
//!
//! Mirrors the SQLite backend's observable behavior (ordering, duplicate
//! root rejection, cascade on root removal) so tests can run either
//! backend against the same expectations. Path refresh tracks (hash →
//! paths) so moved-file refreshes surface without a playlist store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use error_stack::Report;

use automixah_engine::timeline::types::TrackHash;

use super::{IndexedFile, LibraryEntry, LibraryRoot, LibraryStore, LibraryStoreError};

/// In-memory library store: root files keyed by `(root_id, rel_path)`,
/// kept in BTreeMap so listing order matches SQLite's ORDER BY.
#[derive(Default)]
pub struct InMemoryLibraryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_root_id: i64,
    roots: BTreeMap<i64, LibraryRoot>,
    /// (root_id, rel_path string) → entry.
    files: BTreeMap<(i64, String), LibraryEntry>,
    /// Paths recorded by `refresh_track_path`, keyed by hash.
    refreshed_paths: Vec<(TrackHash, String)>,
}

impl InMemoryLibraryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recorded `(hash, path)` refresh pairs (test assertions).
    #[must_use]
    pub fn refreshed_paths(&self) -> Vec<(TrackHash, String)> {
        self.inner
            .lock()
            .map(|inner| inner.refreshed_paths.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl LibraryStore for InMemoryLibraryStore {
    async fn list_roots(&self) -> Result<Vec<LibraryRoot>, Report<LibraryStoreError>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        Ok(inner.roots.values().cloned().collect())
    }

    async fn list_entries(&self) -> Result<Vec<LibraryEntry>, Report<LibraryStoreError>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        Ok(inner.files.values().cloned().collect())
    }

    async fn add_root(&self, path: &str) -> Result<LibraryRoot, Report<LibraryStoreError>> {
        if path.trim().is_empty() {
            return Err(
                Report::new(LibraryStoreError).attach("library root path must not be empty")
            );
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        if inner
            .roots
            .values()
            .any(|root| root.path == Path::new(path))
        {
            return Err(Report::new(LibraryStoreError).attach("duplicate root path"));
        }
        inner.next_root_id += 1;
        let root = LibraryRoot {
            id: inner.next_root_id,
            path: PathBuf::from(path),
        };
        inner.roots.insert(root.id, root.clone());
        Ok(root)
    }

    async fn remove_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        if inner.roots.remove(&root_id).is_none() {
            return Err(Report::new(LibraryStoreError).attach("no such root"));
        }
        inner.files.retain(|(id, _), _| *id != root_id);
        Ok(())
    }

    async fn indexed_files(&self) -> Result<Vec<IndexedFile>, Report<LibraryStoreError>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        Ok(inner
            .files
            .values()
            .map(|entry| IndexedFile {
                root_id: entry.root_id,
                rel_path: entry.rel_path.clone(),
                mtime_secs: entry.mtime_secs,
                size_bytes: entry.size_bytes,
            })
            .collect())
    }

    async fn upsert_file(&self, file: &LibraryEntry) -> Result<(), Report<LibraryStoreError>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        inner
            .files
            .insert((file.root_id, rel_string(&file.rel_path)), file.clone());
        Ok(())
    }

    async fn upsert_files(
        &self,
        batch: &[(LibraryEntry, String)],
    ) -> Result<(), Report<LibraryStoreError>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        for (file, abs_path) in batch {
            inner
                .files
                .insert((file.root_id, rel_string(&file.rel_path)), file.clone());
            inner
                .refreshed_paths
                .push((file.hash.clone(), abs_path.clone()));
        }
        Ok(())
    }

    async fn delete_files(&self, rows: &[IndexedFile]) -> Result<(), Report<LibraryStoreError>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        for row in rows {
            inner
                .files
                .remove(&(row.root_id, rel_string(&row.rel_path)));
        }
        Ok(())
    }

    async fn delete_files_for_root(&self, root_id: i64) -> Result<(), Report<LibraryStoreError>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        inner.files.retain(|(id, _), _| *id != root_id);
        Ok(())
    }

    async fn refresh_track_path(
        &self,
        hash: &TrackHash,
        path: &str,
    ) -> Result<bool, Report<LibraryStoreError>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| Report::new(LibraryStoreError).attach("store mutex poisoned"))?;
        inner.refreshed_paths.push((hash.clone(), path.to_owned()));
        // No playlist rows exist in this backend; report whether any
        // indexed file carries the hash so tests can assert reachability.
        Ok(inner.files.values().any(|entry| entry.hash == *hash))
    }

    fn name(&self) -> &'static str {
        "in-memory"
    }
}

/// `rel_path` as a map key string.
fn rel_string(rel_path: &Path) -> String {
    rel_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(root_id: i64, rel: &str, hash: &str) -> LibraryEntry {
        LibraryEntry {
            root_id,
            rel_path: PathBuf::from(rel),
            hash: TrackHash(hash.to_owned()),
            title: format!("Title {hash}"),
            artist: "Artist".to_owned(),
            duration: Some(61.0),
            bpm: None,
            key: None,
            mtime_secs: 100,
            size_bytes: 2048,
        }
    }

    // Given an empty store.
    // When a root is added and a file upserted.
    // Then both round-trip.
    #[tokio::test]
    async fn add_root_and_upsert_round_trip() {
        let store = InMemoryLibraryStore::new();
        let root = store.add_root("/music").await.expect("root");
        store
            .upsert_file(&entry(root.id, "one.flac", "h1"))
            .await
            .expect("upsert");

        assert_eq!(store.list_roots().await.expect("roots"), vec![root]);
        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash.0, "h1");
    }

    // Given a root.
    // When the same path is added again.
    // Then it is rejected.
    #[tokio::test]
    async fn duplicate_root_path_is_rejected() {
        let store = InMemoryLibraryStore::new();
        store.add_root("/music").await.expect("first");
        assert!(store.add_root("/music").await.is_err());
    }

    // Given two roots.
    // When one is removed.
    // Then only its files disappear.
    #[tokio::test]
    async fn remove_root_cascades_to_its_files() {
        let store = InMemoryLibraryStore::new();
        let a = store.add_root("/a").await.expect("a");
        let b = store.add_root("/b").await.expect("b");
        store
            .upsert_file(&entry(a.id, "one.flac", "h1"))
            .await
            .expect("a one");
        store
            .upsert_file(&entry(b.id, "two.flac", "h2"))
            .await
            .expect("b two");

        store.remove_root(a.id).await.expect("remove");

        let entries = store.list_entries().await.expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].root_id, b.id);
    }
}
