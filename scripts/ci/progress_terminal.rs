use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
const BINARY: &str = env!("CARGO_BIN_EXE_progress-smoke");

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kalisto-progress-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("create isolated fixture");
        for index in 0..3 {
            std::fs::write(root.join(format!("source-{index}")), [index as u8; 4096])
                .expect("write fixture file");
        }
        Self(root)
    }

    fn terminal(&self, arguments: &[&str], environment: &[(&str, &str)]) -> (bool, String) {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open native terminal");
        let mut command = CommandBuilder::new(BINARY);
        command.arg(&self.0);
        command.args(arguments);
        for key in ["CI", "RUST_LOG", "NO_COLOR"] {
            command.env_remove(key);
        }
        command.env("TERM", "xterm-256color");
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("clone terminal reader");
        let reading = thread::spawn(move || {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .expect("read terminal output");
            String::from_utf8(bytes).expect("terminal output is UTF-8")
        });
        let mut child = pair.slave.spawn_command(command).expect("spawn renderer");
        drop(pair.slave);
        let success = child.wait().expect("wait for renderer").success();
        drop(pair.master);
        let output = reading.join().expect("terminal reader completed");
        println!("native terminal transcript: {output:?}");
        (success, output)
    }

    fn piped(&self, arguments: &[&str]) -> std::process::Output {
        Command::new(BINARY)
            .arg(&self.0)
            .args(arguments)
            .env_remove("RUST_LOG")
            .env_remove("CI")
            .env_remove("NO_COLOR")
            .env("TERM", "xterm-256color")
            .output()
            .expect("run with piped output")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove isolated fixture");
    }
}

#[test]
fn terminal_progress_reaches_the_actual_item_total_and_preserves_warnings() {
    let fixture = Fixture::new();
    let (success, output) = fixture.terminal(&[], &[]);
    assert!(success, "{output:?}");
    assert!(
        output.contains("\u{1b}["),
        "no terminal graphics: {output:?}"
    );
    assert!(
        output.contains("3/3"),
        "missing completed count: {output:?}"
    );
    assert!(output.contains("visible warning during copy"), "{output:?}");
    assert!(!output.contains("diagnostic details"), "{output:?}");
    for index in 0..3 {
        assert_eq!(
            std::fs::read(fixture.0.join(format!("destination-{index}"))).unwrap(),
            std::fs::read(fixture.0.join(format!("source-{index}"))).unwrap()
        );
    }
}

#[test]
fn failed_work_does_not_display_a_completed_bar() {
    let fixture = Fixture::new();
    let (success, output) = fixture.terminal(&["--fail"], &[]);
    assert!(!success, "{output:?}");
    assert!(output.contains("copy rejected by fixture"), "{output:?}");
    assert!(!output.contains("3/3"), "false completion: {output:?}");
    assert!(!fixture.0.join("destination-1").exists());
}

#[test]
fn pipes_preserve_plain_diagnostics_without_ansi_and_keep_stdout_clean() {
    let output = Fixture::new().piped(&[]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "copied=3");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains("diagnostic details"), "{stderr:?}");
    assert!(stderr.contains("visible warning during copy"), "{stderr:?}");
}

#[test]
fn json_output_remains_machine_readable_in_a_terminal() {
    let fixture = Fixture::new();
    let (success, output) = fixture.terminal(&["--json"], &[]);
    assert!(success, "{output:?}");
    assert!(!output.contains('\u{1b}'), "{output:?}");
    let json = output.lines().find(|line| line.starts_with('{')).unwrap();
    let value: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(value["copied"], 3);
}

#[test]
fn explicit_logging_shows_diagnostics_instead_of_progress() {
    let fixture = Fixture::new();
    let (success, output) = fixture.terminal(&["--verbose"], &[]);
    assert!(success, "{output:?}");
    assert!(output.contains("diagnostic details"), "{output:?}");
    assert!(!output.contains("3/3"), "{output:?}");
}

#[test]
fn dumb_terminals_and_ci_use_plain_output() {
    for environment in [[("TERM", "dumb")], [("CI", "true")]] {
        let fixture = Fixture::new();
        let (success, output) = fixture.terminal(&[], &environment);
        assert!(success, "{output:?}");
        assert!(!output.contains('\u{1b}'), "{output:?}");
        assert!(output.contains("diagnostic details"), "{output:?}");
    }
}

#[test]
fn monochrome_progress_keeps_counts_without_color_control_sequences() {
    let fixture = Fixture::new();
    let (success, output) = fixture.terminal(&[], &[("NO_COLOR", "1")]);
    assert!(success, "{output:?}");
    assert!(output.contains("3/3"), "{output:?}");
    for sequence in output.split("\u{1b}[").skip(1) {
        assert_ne!(
            sequence.bytes().find(u8::is_ascii_alphabetic),
            Some(b'm'),
            "unexpected color sequence: {output:?}"
        );
    }
}
