use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::error::{Error, Result};

const DEFAULT_FILTER_DIRECTIVE: &str = "info";

pub fn init() -> Result<()> {
    let env_filter = env_filter_from_env()?;
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(false).compact())
        .try_init()
        .map_err(|error| {
            Error::InvalidArgs(format!("failed to initialize tracing subscriber: {error}"))
        })?;
    Ok(())
}

pub fn raw_streams_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

fn env_filter_from_env() -> Result<EnvFilter> {
    match std::env::var("RUST_LOG") {
        Ok(value) => env_filter_from_directive(&value).or_else(|_| default_env_filter()),
        Err(std::env::VarError::NotPresent) => default_env_filter(),
        Err(error) => Err(Error::InvalidArgs(format!(
            "failed to read RUST_LOG: {error}"
        ))),
    }
}

fn env_filter_from_directive(directive: &str) -> Result<EnvFilter> {
    EnvFilter::try_new(directive).map_err(|error| {
        Error::InvalidArgs(format!("invalid tracing filter `{directive}`: {error}"))
    })
}

fn default_env_filter() -> Result<EnvFilter> {
    env_filter_from_directive(DEFAULT_FILTER_DIRECTIVE)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_FILTER_DIRECTIVE, default_env_filter, env_filter_from_directive};

    #[test]
    fn default_filter_is_valid() {
        let filter = default_env_filter().expect("default filter should parse");
        assert!(filter.to_string().contains(DEFAULT_FILTER_DIRECTIVE));
    }

    #[test]
    fn custom_filter_is_valid() {
        let filter = env_filter_from_directive("kalisto=debug,kalisto::cluster::manager=trace")
            .expect("custom filter should parse");
        let rendered = filter.to_string();
        assert!(rendered.contains("kalisto=debug"));
    }
}
