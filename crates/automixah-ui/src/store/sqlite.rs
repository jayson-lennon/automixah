//! SQLite-backed [`GridStore`] over a `daow` pool.
//!
//! Single statements use `pool.execute`/`pool.query_one`; migrations run
//! once at startup through [`super::run_migrations`]. The schema lives in
//! the `automixah-schema` leaf crate.

use std::path::Path;

use async_trait::async_trait;
use daow::{FromRow, Pool};
use error_stack::{Report, ResultExt as _};

use automixah_engine::timeline::types::TrackHash;

use super::{GridOverride, GridStore, GridStoreError};

/// Number of pooled connections. Store IO is tiny; 4 matches jinn's
/// default and comfortably covers UI-frame-sized bursts.
const MAX_POOL_SIZE: usize = 4;

/// SQLite-backed store over `library.sqlite`.
#[derive(Clone)]
pub struct SqliteGridStore {
    pool: Pool,
}

/// Row shape for `SELECT … FROM beat_grids`.
#[derive(Debug, Clone)]
struct GridRow {
    grid_bpm: f64,
    anchor_seconds: f64,
    downbeat_phase: i64,
    updated_at: i64,
    key_root: Option<i64>,
    key_mode: Option<i64>,
}

impl FromRow for GridRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            grid_bpm: row.get("grid_bpm")?,
            anchor_seconds: row.get("anchor_seconds")?,
            downbeat_phase: row.get("downbeat_phase")?,
            updated_at: row.get("updated_at")?,
            key_root: row.get("key_root")?,
            key_mode: row.get("key_mode")?,
        })
    }
}

impl From<GridRow> for GridOverride {
    fn from(row: GridRow) -> Self {
        Self {
            grid_bpm: row.grid_bpm as f32,
            anchor_seconds: row.anchor_seconds as f32,
            downbeat_phase: row.downbeat_phase as u8,
            updated_at: row.updated_at,
            key: decode_key(row.key_root, row.key_mode),
        }
    }
}

/// Decodes the nullable key columns into a [`djcore::key::Key`].
///
/// `key_root` is validated to 0..=11 and `key_mode` to 0 (major) / 1
/// (minor); anything else reads as `None` rather than erroring — a
/// hand-edited database degrades to "key unknown", not a broken library.
pub fn decode_key(key_root: Option<i64>, key_mode: Option<i64>) -> Option<djcore::key::Key> {
    let root = key_root?;
    let mode = key_mode?;
    if !(0..=11).contains(&root) || !(0..=1).contains(&mode) {
        return None;
    }
    #[expect(clippy::cast_possible_truncation, reason = "validated to 0..=11")]
    let root = root as u8;
    let mode = if mode == 0 {
        djcore::key::KeyMode::Major
    } else {
        djcore::key::KeyMode::Minor
    };
    Some(djcore::key::Key { root, mode })
}

/// Encodes a [`djcore::key::Key`] back into the nullable columns.
fn key_to_columns(key: Option<&djcore::key::Key>) -> (Option<i64>, Option<i64>) {
    key.map_or((None, None), |k| {
        (
            Some(i64::from(k.root)),
            Some(match k.mode {
                djcore::key::KeyMode::Major => 0,
                djcore::key::KeyMode::Minor => 1,
            }),
        )
    })
}

impl SqliteGridStore {
    /// Opens (or creates) the database at `path` and runs migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created, the
    /// pool cannot connect, or migrations fail.
    pub async fn open_or_create(path: &Path) -> Result<Self, Report<GridStoreError>> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .change_context(GridStoreError)
                .attach("failed to create parent directory for database file")?;
        }

        let pool = {
            let mut builder = Pool::builder()
                .path(path.to_string_lossy().to_string())
                .max_size(MAX_POOL_SIZE);
            builder = builder.pragma("journal_mode", "WAL");
            builder = builder.pragma("foreign_keys", "ON");
            builder = builder.pragma("busy_timeout", "5000");
            builder.build()
        }
        .change_context(GridStoreError)
        .attach("failed to create grid library connection pool")?;

        super::run_migrations(&pool)
            .await
            .change_context(GridStoreError)
            .attach("failed to migrate grid library")?;

        Ok(Self { pool })
    }

    /// Exposes the pool for future sibling stores sharing `library.sqlite`.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}

#[async_trait]
impl GridStore for SqliteGridStore {
    async fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>> {
        let row: Option<GridRow> = self
            .pool
            .query_one(
                "SELECT grid_bpm, anchor_seconds, downbeat_phase, updated_at, key_root, key_mode \
                 FROM beat_grids WHERE track_hash = ?",
                vec![Box::new(hash.0.clone())],
            )
            .await
            .change_context(GridStoreError)
            .attach("failed to load grid override")?;

        Ok(row.map(GridOverride::from))
    }

    async fn put(
        &self,
        hash: &TrackHash,
        grid: &GridOverride,
    ) -> Result<(), Report<GridStoreError>> {
        // `key: None` means "leave the stored key alone": COALESCE keeps the
        // existing columns when the incoming value is NULL, so a manual grid
        // edit never clobbers a key written by analysis.
        let (key_root, key_mode) = key_to_columns(grid.key.as_ref());
        self.pool
            .execute(
                "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at, key_root, key_mode) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(track_hash) DO UPDATE SET \
                 grid_bpm = excluded.grid_bpm, \
                 anchor_seconds = excluded.anchor_seconds, \
                 downbeat_phase = excluded.downbeat_phase, \
                 updated_at = excluded.updated_at, \
                 key_root = COALESCE(excluded.key_root, beat_grids.key_root), \
                 key_mode = COALESCE(excluded.key_mode, beat_grids.key_mode)",
                vec![
                    Box::new(hash.0.clone()),
                    Box::new(f64::from(grid.grid_bpm)),
                    Box::new(f64::from(grid.anchor_seconds)),
                    Box::new(i64::from(grid.downbeat_phase)),
                    Box::new(grid.updated_at),
                    Box::new(key_root),
                    Box::new(key_mode),
                ],
            )
            .await
            .change_context(GridStoreError)
            .attach("failed to save grid override")?;
        Ok(())
    }

    async fn delete(&self, hash: &TrackHash) -> Result<(), Report<GridStoreError>> {
        self.pool
            .execute(
                "DELETE FROM beat_grids WHERE track_hash = ?",
                vec![Box::new(hash.0.clone())],
            )
            .await
            .change_context(GridStoreError)
            .attach("failed to delete grid override")?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_override() -> GridOverride {
        GridOverride {
            grid_bpm: 139.984,
            anchor_seconds: 0.313,
            downbeat_phase: 2,
            updated_at: 1_700_000_123,
            key: Some(djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            }),
        }
    }

    // Given a SQLite store in a temp file.
    // When an override is saved and loaded.
    // Then the round-trip preserves every field.
    #[tokio::test]
    async fn sqlite_round_trips_override() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteGridStore::open_or_create(&dir.path().join("lib.sqlite"))
            .await
            .expect("open store");

        let hash = TrackHash("cafe01".to_owned());
        let grid = grid_override();

        store.put(&hash, &grid).await.expect("save");
        assert_eq!(store.get(&hash).await.expect("load"), Some(grid));
    }

    // Given a saved override.
    // When the same hash is saved again with different values.
    // Then the upsert replaces the row (one row, new values).
    #[tokio::test]
    async fn sqlite_upsert_replaces() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteGridStore::open_or_create(&dir.path().join("lib.sqlite"))
            .await
            .expect("open store");

        let hash = TrackHash("cafe02".to_owned());
        let first = GridOverride {
            grid_bpm: 140.0,
            anchor_seconds: 0.0,
            downbeat_phase: 0,
            updated_at: 1,
            key: None,
        };
        let second = GridOverride {
            grid_bpm: 138.0,
            anchor_seconds: 0.9,
            downbeat_phase: 3,
            updated_at: 2,
            key: None,
        };

        store.put(&hash, &first).await.expect("first save");
        store.put(&hash, &second).await.expect("upsert");

        let rows = store
            .pool
            .query_one::<i64>("SELECT COUNT(*) AS cnt FROM beat_grids", vec![])
            .await
            .expect("count rows")
            .expect("row present");
        assert_eq!(rows, 1, "upsert keeps a single row");
        assert_eq!(store.get(&hash).await.expect("load"), Some(second));
    }

    // Given a stored key.
    // When a manual grid edit is saved with key: None.
    // Then the stored key survives the upsert.
    #[tokio::test]
    async fn grid_upsert_preserves_stored_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteGridStore::open_or_create(&dir.path().join("lib.sqlite"))
            .await
            .expect("open store");

        let hash = TrackHash("cafe0k".to_owned());
        let analyzed = grid_override();
        store.put(&hash, &analyzed).await.expect("save analyzed");

        let manual_edit = GridOverride {
            grid_bpm: 140.0,
            anchor_seconds: 0.5,
            downbeat_phase: 1,
            updated_at: analyzed.updated_at + 1,
            key: None,
        };
        store.put(&hash, &manual_edit).await.expect("save edit");

        let reloaded = store.get(&hash).await.expect("load").expect("row");
        assert_eq!(reloaded.grid_bpm, 140.0, "grid values replaced");
        assert_eq!(reloaded.key, analyzed.key, "key preserved through edit");
    }

    // Given an empty library.
    // When reopening the same database file.
    // Then migrations are idempotent and the store still works.
    #[tokio::test]
    async fn sqlite_reopen_is_stable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("lib.sqlite");

        let hash = TrackHash("cafe03".to_owned());
        let grid = GridOverride {
            grid_bpm: 150.0,
            anchor_seconds: 0.05,
            downbeat_phase: 1,
            updated_at: 9,
            key: None,
        };

        let first = SqliteGridStore::open_or_create(&path)
            .await
            .expect("open 1");
        first.put(&hash, &grid).await.expect("save");

        let second = SqliteGridStore::open_or_create(&path)
            .await
            .expect("open 2");
        assert_eq!(second.get(&hash).await.expect("reload"), Some(grid));
    }
}
