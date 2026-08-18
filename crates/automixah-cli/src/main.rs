//! Thin binary entry over the library pipeline.

fn main() {
    let config = automixah_cli::cli_config();
    match automixah_cli::run(&config) {
        Ok(_) => {}
        Err(report) => {
            eprintln!("error: {report:#}");
            std::process::exit(1);
        }
    }
}
