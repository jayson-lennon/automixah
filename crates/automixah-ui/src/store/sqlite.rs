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
}

impl FromRow for GridRow {
    fn from_row(row: &daow::Row) -> daow::Result<Self> {
        Ok(Self {
            grid_bpm: row.get("grid_bpm")?,
            anchor_seconds: row.get("anchor_seconds")?,
            downbeat_phase: row.get("downbeat_phase")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

impl From<GridRow> for GridOverride {
    fn from(row: GridRow) -> Self {
        Self {
            #[expect(clippy::cast_possible_truncation, reason = "SQLite REAL is f64")]
            grid_bpm: row.grid_bpm as f32,
            #[expect(clippy::cast_possible_truncation, reason = "SQLite REAL is f64")]
            anchor_seconds: row.anchor_seconds as f32,
            #[expect(clippy::cast_possible_truncation, reason = "phase is 0..=3")]
            downbeat_phase: row.downbeat_phase as u8,
            updated_at: row.updated_at,
        }
    }
}

impl SqliteGridStore {
    /// Opens (or creates) the database at `path` and runs migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory cannot be created, the
    /// pool cannot connect, or migrations fail.
    pub async fn open_or_create(path: &Path) -> Result<Self, Report<GridStoreError>> {
        if let Some(parent) = path.parent() {
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
    #[cfg_attr(not(test), allow(dead_code, reason = "future sibling stores"))]
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
                "SELECT grid_bpm, anchor_seconds, downbeat_phase, updated_at \
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
        self.pool
            .execute(
                "INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at) \
                 VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT(track_hash) DO UPDATE SET \
                 grid_bpm = excluded.grid_bpm, \
                 anchor_seconds = excluded.anchor_seconds, \
                 downbeat_phase = excluded.downbeat_phase, \
                 updated_at = excluded.updated_at",
                vec![
                    Box::new(hash.0.clone()),
                    Box::new(f64::from(grid.grid_bpm)),
                    Box::new(f64::from(grid.anchor_seconds)),
                    Box::new(i64::from(grid.downbeat_phase)),
                    Box::new(grid.updated_at),
                ],
            )
            .await
            .change_context(GridStoreError)
            .attach("failed to save grid override")?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let grid = GridOverride {
            grid_bpm: 139.984,
            anchor_seconds: 0.313,
            downbeat_phase: 2,
            updated_at: 1_700_000_123,
        };

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
        };
        let second = GridOverride {
            grid_bpm: 138.0,
            anchor_seconds: 0.9,
            downbeat_phase: 3,
            updated_at: 2,
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
