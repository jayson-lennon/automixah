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
#[error("automixah ui error")]
pub struct UiError;

fn main() {
    if let Err(report) = run() {
        eprintln!("error: {report:#}");
        std::process::exit(1);
    }
}

/// Assembles services and runs the eframe app.
///
/// All backend construction happens inside one block expression so the
/// final `Services` is immutable and the tokio runtime never escapes.
fn run() -> Result<(), Report<UiError>> {
    let services = {
        let paths = AppPaths::resolve().change_context(UiError)?;

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

        Services {
            grid_store: GridStoreService::new(std::sync::Arc::new(store)),
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
        Box::new(|_cc| Ok(Box::new(AutomixahUiApp::new(services)))),
    )
    .map_err(|e| Report::new(UiError).attach(e.to_string()))
    .attach("run eframe app")
}
