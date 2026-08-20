//! automixah-ui — manual beat-grid alignment tool.
//!
//! egui desktop app: Mixxx-style waveform, grid editing, vinyl-style scrub
//! audition, SQLite grid library. Entry point assembles the [`Services`]
//! container and hands it to eframe.

use automixah_ui_lib::app::AutomixahUiApp;
use automixah_ui_lib::services::{AppPaths, Services};
use automixah_ui_lib::store::GridStoreService;
use automixah_ui_lib::store::sqlite::SqliteGridStore;

use error_stack::{Report, ResultExt as _};

#[derive(Debug, wherror::Error)]
#[error(debug)]
pub struct UiError;

fn main() {
    if let Err(report) = run() {
        eprintln!("error: {report:#?}");
        std::process::exit(1);
    }
}

/// Assembles services and runs the eframe app.
///
/// All backend construction happens inside one block expression so the
/// final `Services` is immutable and the tokio runtime never escapes.
fn run() -> Result<(), Report<UiError>> {
    let services = {
        let paths = AppPaths::resolve()
            .change_context(UiError)
            .attach("failed to resolve application paths")?;

        // Arc'd so the runtime outlives this block: the app spawns
        // load/save tasks for the whole session via `Services::runtime`.
        let runtime = std::sync::Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .change_context(UiError)
                .attach("build tokio runtime")?,
        );

        let store = runtime
            .block_on(SqliteGridStore::open_or_create(&paths.library_db))
            .change_context(UiError)
            .attach("open grid library")?;

        // Same database file, same pool: grids, keys, tags, playlists.
        let playlist_store = automixah_ui_lib::playlist::store::sqlite::SqlitePlaylistStore::new(
            store.pool().clone(),
        );

        Services {
            grid_store: GridStoreService::new(std::sync::Arc::new(store)),
            playlist_store: automixah_ui_lib::playlist::store::PlaylistStoreService::new(
                std::sync::Arc::new(playlist_store),
            ),
            analyzer: std::sync::Arc::new(djcore::analyzer::StratumAnalyzer::new()),
            runtime,
            paths,
        }
    };

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("automixah — grid tool"),
        ..eframe::NativeOptions::default()
    };

    eframe::run_native(
        "automixah-ui",
        options,
        Box::new(|cc| {
            let bus = automixah_ui_lib::bus::EventBus::new(cc.egui_ctx.clone());
            let app = AutomixahUiApp::new(services, bus);
            app.spawn_startup_load();
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| Report::new(UiError).attach(e.to_string()))
    .attach("run eframe app")
}
