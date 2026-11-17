//! CI-only executable for exercising the production renderer without cluster backends.

#[path = "../../src/progress.rs"]
mod progress;

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let root = arguments.first().ok_or("missing fixture directory")?;
    let root = PathBuf::from(root);
    let json = arguments.iter().any(|argument| argument == "--json");
    let verbose = arguments.iter().any(|argument| argument == "--verbose");
    let fail = arguments.iter().any(|argument| argument == "--fail");
    progress::init(
        EnvFilter::new(if verbose { "debug" } else { "info" }),
        !json,
        verbose,
    )?;
    let checking = progress::Progress::spinner("Checking fixture files");
    std::fs::metadata(root.join("source-0"))?;
    checking.finish("Fixture files checked");
    let copying = progress::Progress::bar("Copying fixture files", 3);
    for index in 0..3 {
        copying.set_message(&format!("Copying file {}", index + 1));
        if fail && index == 1 {
            return Err(std::io::Error::other("copy rejected by fixture").into());
        }
        std::fs::copy(
            root.join(format!("source-{index}")),
            root.join(format!("destination-{index}")),
        )?;
        copying.inc(1);
    }
    tracing::info!("diagnostic details");
    tracing::warn!("visible warning during copy");
    copying.finish("Files copied");
    if json {
        println!("{{\"copied\":3}}");
    } else {
        println!("copied=3");
    }
    Ok(())
}
