use std::path::PathBuf;

use kalisto::probe::macos_lima::{ProbeConfig, ProbeStatus, run};

fn live_tests_enabled() -> bool {
    std::env::var("KALISTO_LIVE_LIMA_TESTS").is_ok_and(|value| value == "1")
}

#[test]
fn macos_lima_probe() {
    if !live_tests_enabled() {
        eprintln!("skipping live macOS Lima probe; set KALISTO_LIVE_LIMA_TESTS=1");
        return;
    }

    let mut config = ProbeConfig::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    if let Ok(template) = std::env::var("KALISTO_LIMA_TEMPLATE") {
        config.template = template;
    }
    if let Ok(network) = std::env::var("KALISTO_LIMA_NETWORK") {
        config.network = network;
    }
    if std::env::var("KALISTO_LIMA_NO_ROSETTA").is_ok_and(|value| value == "1") {
        config.enable_rosetta = false;
    }

    let report = run(&config);
    for result in &report.results {
        eprintln!("[{}] {}: {}", result.status, result.name, result.detail);
    }
    eprintln!("{}", report.summary());

    let failures: Vec<_> = report
        .results
        .iter()
        .filter(|result| result.status == ProbeStatus::Fail)
        .collect();

    assert!(
        failures.is_empty(),
        "live macOS Lima probe had failures: {failures:#?}"
    );
}
