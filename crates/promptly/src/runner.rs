//! Local public-test runner (`19`).
//!
//! `promptly test` prefers running a level's **public** tests on the player's
//! machine for fast iteration without an EC2 round-trip, falling back to the
//! remote Judge0 service when local execution isn't possible. The grading
//! decision mirrors the web staging semantics (`09`/`lib/judge0`): Judge0
//! compares stdout after trimming trailing whitespace (exact match), or within a
//! per-case float `tolerance` when one is given. This module owns the
//! `stdin_stdout` harness (one process per case); suite harnesses
//! (`multi_file`/`http_integration`/…) need the server driver and route remote.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Per-case wall-clock cap for a local run, so a buggy infinite loop can't hang
/// the CLI. The remote Judge0 path enforces the authoritative limits (`08`).
const CASE_TIMEOUT: Duration = Duration::from_secs(15);

/// A parsed `tests/public/*.json` file (the shared artifact `09` executes).
#[derive(Debug, Clone, Deserialize)]
pub struct CaseFile {
    pub harness: String,
    #[serde(default)]
    pub runtime_version: String,
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub cases: Vec<Case>,
}

/// One `stdin_stdout` public test case.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub name: String,
    #[serde(default)]
    pub stdin: String,
    #[serde(default)]
    pub expected_stdout: String,
    /// Optional float tolerance (epsilon compare), matching the staging grader.
    #[serde(default)]
    pub tolerance: Option<f64>,
}

/// The verdict for one case — mirrors the staging `PerTestResult` statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseStatus {
    Passed,
    Failed,
    /// Compilation/runtime error, timeout, or a runner problem.
    Errored,
}

/// One case's result.
#[derive(Debug, Clone)]
pub struct CaseResult {
    pub name: String,
    pub status: CaseStatus,
    /// A short, non-spoiler note (wrong-answer summary, error head, …).
    pub detail: Option<String>,
}

/// A locally-runnable `stdin_stdout` runtime. Compiled languages without a
/// single source-run command (Rust/C/C++) and TypeScript route to remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRuntime {
    Go,
    Python,
    Node,
}

impl LocalRuntime {
    /// Map a manifest `runtime_version` to a local runner, or `None` when local
    /// execution isn't supported for it (→ remote fallback).
    pub fn from_runtime_version(runtime_version: &str) -> Option<Self> {
        let rv = runtime_version.to_lowercase();
        // TypeScript ships as `node20-ts5`; it needs a transpile step, so route
        // it remote rather than the plain Node runner.
        if rv.contains("ts") {
            return None;
        }
        if rv.starts_with("go") {
            Some(Self::Go)
        } else if rv.starts_with("python") {
            Some(Self::Python)
        } else if rv.starts_with("node") {
            Some(Self::Node)
        } else {
            None
        }
    }

    /// The program candidates to probe / invoke (the first that exists wins —
    /// `python` vs `python3`).
    fn programs(self) -> &'static [&'static str] {
        match self {
            Self::Go => &["go"],
            Self::Python => &["python3", "python"],
            Self::Node => &["node"],
        }
    }

    /// Build the run command for `entry` in `workspace`. Go runs the whole
    /// package; Python/Node run the entry file.
    fn command(self, program: &str, workspace: &Path, entry: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(workspace);
        match self {
            Self::Go => {
                cmd.args(["run", "."]);
            }
            Self::Python | Self::Node => {
                cmd.arg(entry);
            }
        }
        cmd
    }

    /// The first installed program for this runtime (probes `--version`), or
    /// `None` when the toolchain isn't on PATH.
    pub fn resolve_program(self) -> Option<&'static str> {
        self.programs()
            .iter()
            .copied()
            .find(|program| probe_program(program))
    }
}

/// Does `<program> --version` run successfully (toolchain installed)?
fn probe_program(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Trim trailing whitespace, matching Judge0's stdout comparison (`09`).
fn trim_trailing(s: &str) -> &str {
    s.trim_end()
}

/// Grade one case's output the way staging does: an exact (trailing-whitespace
/// trimmed) match, or a token-wise float compare within `tolerance` when set.
pub fn grade_case(actual: &str, expected: &str, tolerance: Option<f64>) -> bool {
    match tolerance {
        Some(tol) => matches_within_tolerance(actual, expected, tol),
        None => trim_trailing(actual) == trim_trailing(expected),
    }
}

/// Token-wise comparison: equal token counts, each token exactly equal or (when
/// both numeric) within `tolerance`. Mirrors `matchesWithinTolerance` (`lib/judge0`).
fn matches_within_tolerance(actual: &str, expected: &str, tolerance: f64) -> bool {
    let a: Vec<&str> = actual.split_whitespace().collect();
    let e: Vec<&str> = expected.split_whitespace().collect();
    if a.len() != e.len() {
        return false;
    }
    for (lhs, rhs) in a.iter().zip(e.iter()) {
        if lhs == rhs {
            continue;
        }
        match (lhs.parse::<f64>(), rhs.parse::<f64>()) {
            (Ok(av), Ok(ev)) if (av - ev).abs() <= tolerance => continue,
            _ => return false,
        }
    }
    true
}

/// Run every case in `file` locally with `program`, returning a verdict each.
pub fn run_local(
    runtime: LocalRuntime,
    program: &str,
    workspace: &Path,
    file: &CaseFile,
) -> Vec<CaseResult> {
    file.cases
        .iter()
        .map(|case| run_one(runtime, program, workspace, &file.entry, case))
        .collect()
}

fn run_one(
    runtime: LocalRuntime,
    program: &str,
    workspace: &Path,
    entry: &str,
    case: &Case,
) -> CaseResult {
    let mut command = runtime.command(program, workspace, entry);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match execute(command, &case.stdin) {
        Ok(out) if out.timed_out => CaseResult {
            name: case.name.clone(),
            status: CaseStatus::Errored,
            detail: Some(format!("timed out after {}s", CASE_TIMEOUT.as_secs())),
        },
        Ok(out) if !out.success => CaseResult {
            name: case.name.clone(),
            status: CaseStatus::Errored,
            detail: Some(error_head(&out.stderr, &out.stdout)),
        },
        Ok(out) => {
            if grade_case(&out.stdout, &case.expected_stdout, case.tolerance) {
                CaseResult {
                    name: case.name.clone(),
                    status: CaseStatus::Passed,
                    detail: None,
                }
            } else {
                CaseResult {
                    name: case.name.clone(),
                    status: CaseStatus::Failed,
                    detail: Some(diff_summary(&case.expected_stdout, &out.stdout)),
                }
            }
        }
        Err(err) => CaseResult {
            name: case.name.clone(),
            status: CaseStatus::Errored,
            detail: Some(format!("failed to run: {err}")),
        },
    }
}

/// The captured result of one local process run.
struct RunOutput {
    success: bool,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

/// Spawn `command`, feed `stdin`, and wait up to [`CASE_TIMEOUT`], killing a
/// process that overruns. Output is read after exit (public-test output is tiny).
fn execute(mut command: Command, stdin: &str) -> std::io::Result<RunOutput> {
    let mut child = command.spawn()?;

    // Feed stdin and drain stdout/stderr on their own threads. Writing all of
    // stdin synchronously and reading output only after exit deadlocks any child
    // that fills its stdout pipe (~64 KiB) before consuming stdin: the child
    // blocks writing stdout while we block in `write_all(stdin)`, and the timeout
    // loop below — which is what would kill it — is never reached. With the I/O
    // off the main thread the loop stays responsive, and a kill breaks the
    // writer's pipe so it unblocks too.
    let stdin_writer = child.stdin.take().map(|mut sink| {
        let bytes = stdin.as_bytes().to_vec();
        std::thread::spawn(move || {
            // A program that exits before reading stdin is graded on what it
            // printed, so a broken pipe is expected here, not an error. Dropping
            // `sink` closes stdin, so a child that reads to EOF still proceeds.
            let _ = sink.write_all(&bytes);
        })
    });
    let stdout_reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = out.read_to_string(&mut buf);
            buf
        })
    });
    let stderr_reader = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = err.read_to_string(&mut buf);
            buf
        })
    });

    let start = Instant::now();
    let mut exit_status = None;
    let timed_out = loop {
        match child.try_wait()? {
            Some(status) => {
                exit_status = Some(status);
                break false;
            }
            None => {
                if start.elapsed() >= CASE_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    // The child has exited (or been killed), so both output pipes are closed and
    // the readers see EOF; the writer finishes once stdin is consumed or broken.
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    let stdout = stdout_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    Ok(RunOutput {
        success: !timed_out && exit_status.map(|s| s.success()).unwrap_or(false),
        timed_out,
        stdout,
        stderr,
    })
}

/// A short, non-spoiler head of an error (compiler/runtime output).
fn error_head(stderr: &str, stdout: &str) -> String {
    let source = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let head = source.lines().take(3).collect::<Vec<_>>().join(" / ");
    truncate(&head, 200)
}

/// A non-spoiler wrong-answer summary (lengths only, never the expected value).
fn diff_summary(expected: &str, actual: &str) -> String {
    format!(
        "expected {} byte(s) of output, got {}",
        trim_trailing(expected).len(),
        trim_trailing(actual).len(),
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_mapping_supports_go_python_node_only() {
        assert_eq!(
            LocalRuntime::from_runtime_version("go1.22"),
            Some(LocalRuntime::Go)
        );
        assert_eq!(
            LocalRuntime::from_runtime_version("python3.11"),
            Some(LocalRuntime::Python)
        );
        assert_eq!(
            LocalRuntime::from_runtime_version("node20"),
            Some(LocalRuntime::Node)
        );
        // TypeScript and compiled languages route remote.
        assert_eq!(LocalRuntime::from_runtime_version("node20-ts5"), None);
        assert_eq!(LocalRuntime::from_runtime_version("rust1.75"), None);
        assert_eq!(LocalRuntime::from_runtime_version("gcc13-c17"), None);
    }

    #[test]
    fn exact_grading_trims_trailing_whitespace_like_judge0() {
        assert!(grade_case("10\n", "10\n", None));
        assert!(grade_case("10", "10\n", None), "trailing newline ignored");
        assert!(grade_case("-1\n2\n3\n  ", "-1\n2\n3", None));
        assert!(!grade_case("11\n", "10\n", None));
        // Internal whitespace still matters.
        assert!(!grade_case("1 2\n", "12\n", None));
    }

    #[test]
    fn tolerance_grading_compares_numbers_within_epsilon() {
        assert!(grade_case("3.14159", "3.14160", Some(0.001)));
        assert!(!grade_case("3.14159", "3.15", Some(0.001)));
        // Token count must match.
        assert!(!grade_case("1.0 2.0", "1.0", Some(0.1)));
        // Non-numeric tokens must match exactly even under tolerance.
        assert!(grade_case("ok 1.00", "ok 1.01", Some(0.1)));
        assert!(!grade_case("yes 1.00", "no 1.00", Some(0.1)));
    }

    #[test]
    fn diff_summary_does_not_leak_the_expected_value() {
        let summary = diff_summary("the-secret-answer\n", "wrong\n");
        assert!(summary.contains("17 byte"), "{summary}");
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn parses_a_stdin_stdout_case_file() {
        let json = r#"{
            "harness":"stdin_stdout","runtime_version":"go1.22","entry":"main.go",
            "cases":[{"name":"basic","stdin":"1\n","expected_stdout":"1\n"},
                     {"name":"approx","stdin":"x","expected_stdout":"3.14","tolerance":0.01}]
        }"#;
        let file: CaseFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.harness, "stdin_stdout");
        assert_eq!(file.cases.len(), 2);
        assert_eq!(file.cases[1].tolerance, Some(0.01));
    }
}
