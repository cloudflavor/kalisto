use std::path::PathBuf;

use kalisto::config::load_project_config;

fn example_root(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

#[test]
fn nginx_smoke_example_loads() {
    let root = example_root("examples/harness/nginx-smoke");
    let config = load_project_config(&root)
        .unwrap()
        .expect("example config should exist");

    assert_eq!(config.harness.scenarios.len(), 1);
    let scenario = &config.harness.scenarios[0];
    assert_eq!(scenario.name, "deployment-smoke");
    assert_eq!(scenario.manifests.len(), 2);
    assert_eq!(scenario.waits.len(), 1);
    assert_eq!(scenario.probes.len(), 3);
}

#[test]
fn job_smoke_example_loads() {
    let root = example_root("examples/harness/job-smoke");
    let config = load_project_config(&root)
        .unwrap()
        .expect("example config should exist");

    assert_eq!(config.harness.scenarios.len(), 1);
    let scenario = &config.harness.scenarios[0];
    assert_eq!(scenario.name, "job-smoke");
    assert_eq!(scenario.manifests.len(), 1);
    assert_eq!(scenario.waits.len(), 1);
    assert_eq!(scenario.probes.len(), 2);
}
