use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|source| Error::CommandIo {
                program: program.to_owned(),
                source,
            })?;

        let stdout = String::from_utf8(output.stdout)?;
        let stderr = String::from_utf8(output.stderr)?;

        if output.status.success() {
            Ok(Output { stdout, stderr })
        } else {
            Err(Error::CommandFailed {
                program: program.to_owned(),
                args: args.to_vec(),
                code: output.status.code(),
                stdout,
                stderr,
            })
        }
    }
}

pub fn ensure_path_exists(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(Error::MissingPath(path.to_path_buf()))
    }
}
