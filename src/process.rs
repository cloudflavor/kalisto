use std::path::Path;
use std::process::{Command, Stdio};
use std::{
    io::{Read, Write},
    thread,
};

use crate::error::{Error, Result};
use crate::telemetry::raw_streams_enabled;

#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;

    fn run_live(&self, program: &str, args: &[String]) -> Result<Output> {
        self.run(program, args)
    }

    fn system_runner(&self) -> Option<SystemRunner> {
        None
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        run_command(program, args, false)
    }

    fn run_live(&self, program: &str, args: &[String]) -> Result<Output> {
        run_command(program, args, raw_streams_enabled())
    }

    fn system_runner(&self) -> Option<SystemRunner> {
        Some(*self)
    }
}

pub fn ensure_path_exists(path: &Path) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(Error::MissingPath(path.to_path_buf()))
    }
}

fn run_command(program: &str, args: &[String], live: bool) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args);

    let output = if live {
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::CommandIo {
                program: program.to_owned(),
                source,
            })
            .and_then(|mut child| {
                let stdout = child.stdout.take().ok_or_else(|| {
                    Error::InvalidArgs(format!("failed to capture stdout for `{program}`"))
                })?;
                let stderr = child.stderr.take().ok_or_else(|| {
                    Error::InvalidArgs(format!("failed to capture stderr for `{program}`"))
                })?;
                let stdout_thread = thread::spawn(move || pump_output(stdout, false));
                let stderr_thread = thread::spawn(move || pump_output(stderr, true));
                let status = child.wait().map_err(|source| Error::CommandIo {
                    program: program.to_owned(),
                    source,
                })?;
                let stdout = stdout_thread
                    .join()
                    .map_err(|_| {
                        Error::InvalidArgs(format!("stdout pump thread panicked for `{program}`"))
                    })?
                    .map_err(|source| Error::CommandIo {
                        program: format!("{program}:stdout"),
                        source,
                    })?;
                let stderr = stderr_thread
                    .join()
                    .map_err(|_| {
                        Error::InvalidArgs(format!("stderr pump thread panicked for `{program}`"))
                    })?
                    .map_err(|source| Error::CommandIo {
                        program: format!("{program}:stderr"),
                        source,
                    })?;
                Ok((status.success(), status.code(), stdout, stderr))
            })?
    } else {
        let output = command.output().map_err(|source| Error::CommandIo {
            program: program.to_owned(),
            source,
        })?;
        (
            output.status.success(),
            output.status.code(),
            output.stdout,
            output.stderr,
        )
    };

    let stdout = String::from_utf8(output.2)?;
    let stderr = String::from_utf8(output.3)?;

    if output.0 {
        Ok(Output { stdout, stderr })
    } else {
        Err(Error::CommandFailed {
            program: program.to_owned(),
            args: args.to_vec(),
            code: output.1,
            stdout,
            stderr,
        })
    }
}

fn pump_output<R: Read>(mut reader: R, to_stderr: bool) -> std::io::Result<Vec<u8>> {
    let mut stream_output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        stream_output.extend_from_slice(&buffer[..read]);
        if to_stderr {
            std::io::stderr().write_all(&buffer[..read])?;
            std::io::stderr().flush()?;
        } else {
            std::io::stdout().write_all(&buffer[..read])?;
            std::io::stdout().flush()?;
        }
    }
    Ok(stream_output)
}
