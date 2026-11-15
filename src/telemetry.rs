use tracing::Level;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::error::{Error, Result};

const DEFAULT_FILTER_DIRECTIVE: &str = "info";

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn init(log_level: Option<Level>) -> Result<()> {
    let env_filter = env_filter(log_level)?;
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_target(false)
                .compact()
                .with_writer(std::io::stderr),
        )
        .try_init()
        .map_err(|error| {
            Error::InvalidArgs(format!("failed to initialize tracing subscriber: {error}"))
        })?;
    Ok(())
}

pub fn raw_streams_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

fn env_filter(log_level: Option<Level>) -> Result<EnvFilter> {
    match std::env::var("RUST_LOG") {
        Ok(value) => env_filter_from_directive(&value).or_else(|_| default_env_filter()),
        Err(std::env::VarError::NotPresent) => match log_level {
            Some(level) => env_filter_from_level(level),
            None => default_env_filter(),
        },
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

fn env_filter_from_level(level: Level) -> Result<EnvFilter> {
    env_filter_from_directive(level.as_str())
}

fn default_env_filter() -> Result<EnvFilter> {
    env_filter_from_directive(DEFAULT_FILTER_DIRECTIVE)
}

#[cfg(test)]
mod tests {
    use crate::test_support::env_lock;
    use tracing::Level;

    use super::{
        DEFAULT_FILTER_DIRECTIVE, default_env_filter, env_filter, env_filter_from_directive,
    };

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

    #[test]
    fn cli_log_level_is_used_when_rust_log_is_unset() {
        let _guard = env_lock();
        let original = std::env::var_os("RUST_LOG");
        unsafe { std::env::remove_var("RUST_LOG") };
        let filter = env_filter(Some(Level::DEBUG)).expect("debug log level should parse");
        match original {
            Some(value) => unsafe { std::env::set_var("RUST_LOG", value) },
            None => unsafe { std::env::remove_var("RUST_LOG") },
        }
        assert!(filter.to_string().contains("debug"));
    }
}
