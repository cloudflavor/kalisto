use std::error::Error as StdError;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    CommandIo {
        program: String,
        source: std::io::Error,
    },
    CommandFailed {
        program: String,
        args: Vec<String>,
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    InvalidArgs(String),
    MissingPath(PathBuf),
    Utf8(std::string::FromUtf8Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommandIo { program, source } => {
                write!(f, "failed to execute `{program}`: {source}")
            }
            Self::CommandFailed {
                program,
                args,
                code,
                stdout,
                stderr,
            } => write!(
                f,
                "`{program} {}` failed with exit code {:?}\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                code,
                stdout,
                stderr
            ),
            Self::InvalidArgs(message) => write!(f, "{message}"),
            Self::MissingPath(path) => write!(f, "path does not exist: {}", path.display()),
            Self::Utf8(error) => write!(f, "invalid UTF-8 in command output: {error}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::CommandIo { source, .. } => Some(source),
            Self::Utf8(source) => Some(source),
            Self::CommandFailed { .. } | Self::InvalidArgs(_) | Self::MissingPath(_) => None,
        }
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(value: std::string::FromUtf8Error) -> Self {
        Self::Utf8(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
