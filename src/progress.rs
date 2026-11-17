//! Interactive progress, with ordinary tracing output for automation and diagnostics.

use std::io::{IsTerminal, stderr, stdout};
use std::sync::OnceLock;
use std::time::Duration;

use indicatif::ProgressStyle;
use tracing::Span;
use tracing_indicatif::IndicatifLayer;
use tracing_indicatif::TickSettings;
use tracing_indicatif::filter::IndicatifFilter;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

static STYLES: OnceLock<Styles> = OnceLock::new();

struct Styles {
    spinner: ProgressStyle,
    bar: ProgressStyle,
}

impl Styles {
    fn new(color: bool) -> Result<Self, indicatif::style::TemplateError> {
        let spinner = if color {
            "{spinner:.cyan} {msg} [{elapsed_precise}]"
        } else {
            "{spinner} {msg} [{elapsed_precise}]"
        };
        let bar = if color {
            "{spinner:.cyan} {msg} [{wide_bar:.cyan/blue}] {pos}/{len} [{elapsed_precise}]"
        } else {
            "{spinner} {msg} [{wide_bar}] {pos}/{len} [{elapsed_precise}]"
        };
        let ticks: &[&str] = if cfg!(windows) {
            &["|", "/", "-", "\\", "+"]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]
        };
        let characters = if cfg!(windows) { "=>-" } else { "█▓░" };
        Ok(Self {
            spinner: ProgressStyle::with_template(spinner)?.tick_strings(ticks),
            bar: ProgressStyle::with_template(bar)?
                .tick_strings(ticks)
                .progress_chars(characters),
        })
    }
}

pub(crate) fn init(
    filter: EnvFilter,
    allow_progress: bool,
    explicit_logging: bool,
) -> Result<(), String> {
    let terminal = stdout().is_terminal() && stderr().is_terminal();
    let capable = std::env::var_os("TERM").is_none_or(|term| term != "dumb");
    let ci = std::env::var_os("CI").is_some();
    let color = terminal && capable && !ci && std::env::var_os("NO_COLOR").is_none();
    let interactive = allow_progress && terminal && capable && !explicit_logging && !ci;
    let result = if interactive {
        let styles = Styles::new(color).map_err(|error| error.to_string())?;
        let layer = IndicatifLayer::new()
            .with_progress_style(styles.spinner.clone())
            .with_tick_settings(TickSettings {
                term_draw_hz: 10,
                default_tick_interval: Some(Duration::from_millis(100)),
                ..TickSettings::default()
            });
        let writer = layer.get_stderr_writer();
        STYLES
            .set(styles)
            .map_err(|_| "progress renderer is already initialized".to_owned())?;
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .without_time()
                    .compact()
                    .with_ansi(color)
                    .with_writer(writer)
                    .with_filter(LevelFilter::WARN),
            )
            .with(layer.with_filter(IndicatifFilter::new(false)))
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .compact()
                    .with_ansi(color && allow_progress)
                    .with_writer(stderr),
            )
            .try_init()
    };
    result.map_err(|error| error.to_string())
}

/// A real operation's lifetime; unfinished operations never retain a success marker.
pub(crate) struct Progress {
    span: Span,
}

impl Progress {
    pub(crate) fn spinner(message: &str) -> Self {
        Self::new(message, None)
    }

    pub(crate) fn bar(message: &str, total: u64) -> Self {
        Self::new(message, Some(total))
    }

    fn new(message: &str, total: Option<u64>) -> Self {
        let Some(styles) = STYLES.get() else {
            tracing::info!("{message}");
            return Self { span: Span::none() };
        };
        let span = tracing::info_span!(
            target: "kalisto::progress",
            "progress",
            indicatif.pb_show = tracing::field::Empty
        );
        span.pb_set_style(if total.is_some() {
            &styles.bar
        } else {
            &styles.spinner
        });
        if let Some(total) = total {
            span.pb_set_length(total);
        }
        span.pb_set_message(message);
        span.pb_start();
        Self { span }
    }

    pub(crate) fn set_message(&self, message: &str) {
        if self.span.is_disabled() {
            tracing::info!("{message}");
        } else {
            self.span.pb_set_message(message);
        }
    }

    pub(crate) fn inc(&self, delta: u64) {
        self.span.pb_inc(delta);
    }

    pub(crate) fn finish(self, message: &str) {
        if self.span.is_disabled() {
            tracing::info!("{message}");
        } else {
            self.span.pb_set_finish_message(message);
        }
    }
}
