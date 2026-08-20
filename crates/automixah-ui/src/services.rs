//! The `Services` container — automixah-ui's application-wide DI bundle.
//!
//! Assembled exactly once at startup (in `main.rs`, inside a block
//! expression) and shared by clone throughout the app. Every field is
//! either cheap-to-clone data or a trait-backed service wrapper, per the
//! jinn `AGENTS.md` pattern.

use std::path::PathBuf;

use error_stack::Report;

use crate::store::{CueStoreService, GridStoreService};

/// Application filesystem paths, resolved once at startup.
#[derive(Debug, Clone)]
pub struct AppPaths {
    /// XDG data directory (`~/.local/share/automixah`).
    pub data_dir: PathBuf,
    /// Grid library database (`<data_dir>/library.sqlite`).
    pub library_db: PathBuf,
}

impl AppPaths {
    /// Resolves the default XDG paths, creating the data directory.
    ///
    /// # Errors
    ///
    /// Returns an error if no home data directory can be resolved or the
    /// directory cannot be created.
    pub fn resolve() -> Result<Self, Report<AppPathsError>> {
        let base = dirs::data_dir()
            .ok_or_else(|| Report::new(AppPathsError).attach("no XDG data dir"))
            .map(|d| d.join("automixah"))?;
        std::fs::create_dir_all(&base).map_err(|e| {
            Report::new(AppPathsError)
                .attach(e.to_string())
                .attach(format!("create {}", base.display()))
        })?;
        let library_db = base.join("library.sqlite");
        Ok(Self {
            library_db,
            data_dir: base,
        })
    }

    /// Test paths rooted in a temp dir.
    #[cfg(test)]
    #[must_use]
    pub fn for_test(root: &std::path::Path) -> Self {
        Self {
            data_dir: root.to_owned(),
            library_db: root.join("library.sqlite"),
        }
    }
}

/// Error when application paths cannot be resolved.
#[derive(Debug, wherror::Error)]
#[error(debug)]
pub struct AppPathsError;

/// The application-wide service container.
///
/// Constructed once in `main.rs`; the eframe app holds a clone. Adding a
/// capability means adding a field here — zero call sites change.
#[derive(Clone)]
pub struct Services {
    /// Resolved application paths.
    pub paths: AppPaths,
    /// Grid override persistence (SQLite behind a trait).
    pub grid_store: GridStoreService,
    /// Cue point persistence (SQLite behind a trait).
    pub cue_store: CueStoreService,
    /// Playlist + track-tag persistence (SQLite behind a trait).
    pub playlist_store: crate::playlist::store::PlaylistStoreService,
    /// Analysis backend used by the playlist queue (injected so tests
    /// can swap in `FakeAnalyzer`).
    pub analyzer: std::sync::Arc<dyn djcore::analyzer::AudioAnalyzer>,
    /// Shared async runtime, kept alive for the app's lifetime.
    ///
    /// An `Arc<Runtime>` (not a bare `Handle`): dropping the `Runtime`
    /// shuts it down and leaves every later spawn a silent no-op —
    /// the runtime must outlive the `Services` clones.
    pub runtime: std::sync::Arc<tokio::runtime::Runtime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a fresh temp root.
    // When test paths are resolved.
    // Then the db path sits inside the data dir.
    #[test]
    fn test_paths_nest_db() {
        let dir = tempfile::tempdir().expect("temp");
        let paths = AppPaths::for_test(dir.path());
        assert_eq!(paths.library_db, paths.data_dir.join("library.sqlite"));
    }
}
