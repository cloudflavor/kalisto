pub use crate::k3s::ServerMode;
pub use crate::lima_k3s::config::Config as ProbeConfig;
pub use crate::lima_k3s::report::{
    Report as ProbeReport, Result as ProbeResult, Status as ProbeStatus,
};

use crate::lima_k3s::cluster::{run as run_cluster, run_with_runner as run_cluster_with_runner};
use crate::process::CommandRunner;

pub fn run(config: &ProbeConfig) -> ProbeReport {
    run_cluster(config)
}

pub fn run_with_runner(config: &ProbeConfig, runner: &dyn CommandRunner) -> ProbeReport {
    run_cluster_with_runner(config, runner)
}
