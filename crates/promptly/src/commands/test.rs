//! `promptly test` — run the level's public tests, preferring a fast local run
//! and falling back to a remote run on the server's Judge0 backend
//! (`POST /api/cli/test`, the [`RemoteTests`] seam) when local execution isn't
//! possible (`19`). Both paths render the same per-case report; results match
//! the web pass/fail (`09`).

use std::path::{Path, PathBuf};

use promptlyd::manifest::Manifest;

use crate::cloud::{CloudError, RemoteTestReport, RemoteTests};
use crate::redaction::{self, RedactionError};
use crate::runner::{self, CaseFile, CaseResult, CaseStatus, LocalRuntime};
use crate::style::Style;
use crate::submission;
use crate::visual;
use crate::CommandExit;

/// Conventional location of the public test set inside a workspace (`07`).
const PUBLIC_TESTS_DIR: &str = "tests/public";

pub fn run(
    workspace: &Path,
    manifest: Option<&Manifest>,
    remote: &dyn RemoteTests,
    style: Style,
) -> anyhow::Result<CommandExit> {
    let Some(path) = find_case_file(workspace) else {
        println!(
            "{}",
            style.red(&format!(
                "no public tests found under {PUBLIC_TESTS_DIR}/ — run `promptly init <level>` in this workspace"
            )),
        );
        return Ok(CommandExit::Failure);
    };

    let file: CaseFile = match read_case_file(&path) {
        Ok(file) => file,
        Err(err) => {
            println!("{} {err}", style.red("could not read public tests:"));
            return Ok(CommandExit::Failure);
        }
    };

    // Suite harnesses need the server-side driver; only `stdin_stdout` runs local.
    if file.harness != "stdin_stdout" {
        return remote_fallback(
            workspace,
            manifest,
            remote,
            &format!("the '{}' harness runs server-side", file.harness),
            // Installing a toolchain wouldn't enable a local run — no hint.
            None,
            style,
        );
    }

    let runtime = match LocalRuntime::from_runtime_version(&file.runtime_version) {
        Some(rt) => rt,
        None => {
            return remote_fallback(
                workspace,
                manifest,
                remote,
                &format!(
                    "local execution isn't supported for {} yet",
                    file.runtime_version
                ),
                None,
                style,
            );
        }
    };
    let Some(program) = runtime.resolve_program() else {
        return remote_fallback(
            workspace,
            manifest,
            remote,
            &format!(
                "the {} toolchain isn't installed on PATH",
                file.runtime_version
            ),
            Some(&file.runtime_version),
            style,
        );
    };

    println!(
        "{} {} {}",
        style.dim("running"),
        style.accent(&format!("{} public tests", file.cases.len())),
        style.dim(&format!("locally ({})", file.runtime_version)),
    );
    let results = runner::run_local(runtime, program, workspace, &file);
    let (report, all_passed) = render_results(&results, style);
    print!("{report}");
    Ok(if all_passed {
        CommandExit::Success
    } else {
        CommandExit::Failure
    })
}

/// Local execution isn't possible — run the public tests remotely instead
/// (`POST /api/cli/test`): package the workspace exactly like `submit` (same
/// allowlist, same pre-upload redaction), upload, and render the server's
/// per-case verdicts through the local renderer. `local_hint` names the
/// toolchain that would enable a local run, when installing one would help.
fn remote_fallback(
    workspace: &Path,
    manifest: Option<&Manifest>,
    remote: &dyn RemoteTests,
    reason: &str,
    local_hint: Option<&str>,
    style: Style,
) -> anyhow::Result<CommandExit> {
    println!("{} {reason}", style.yellow("local run unavailable —"));

    // The remote run needs the manifest: the slug to test and the file
    // allowlist to package.
    let Some(manifest) = manifest else {
        println!(
            "{}",
            style.red("not a Promptly workspace — run `promptly init <level>` first"),
        );
        return Ok(CommandExit::Failure);
    };

    let bundle = match submission::gather_submission(workspace, manifest) {
        Ok(bundle) => bundle,
        Err(err) => {
            println!("{} {err}", style.red("couldn't package the workspace:"));
            return Ok(CommandExit::Failure);
        }
    };
    // The same pre-upload redaction as `submit`: nothing leaves the machine
    // with a secret-shaped span in it, even for an unranked test run.
    let redacted = match redaction::redact_bundle(&bundle) {
        Ok(redacted) => redacted,
        Err(RedactionError::Uncleanable(category)) => {
            println!(
                "{} an unredactable {category} block was found in the solution — remove it and retry",
                style.red("blocked:"),
            );
            return Ok(CommandExit::Failure);
        }
    };
    if !redacted.categories.is_empty() {
        println!(
            "{} {}",
            style.yellow("redacted before upload:"),
            redacted.categories.join(", "),
        );
    }

    println!(
        "{} {} {}",
        style.dim("running"),
        style.accent(&format!("{} public tests", manifest.slug)),
        style.dim("remotely (server-side Judge0)"),
    );
    match remote.run_public_tests(&manifest.slug, &redacted.bundle) {
        Ok(report) => {
            let results = remote_case_results(&report);
            let (text, all_passed) = render_results(&results, style);
            print!("{text}");
            // Compile/setup output explains an errored suite; skip it on green.
            if report.crashed || !all_passed {
                if let Some(compile) = report
                    .compile_output
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                {
                    println!("  {}", style.dim("compiler/setup output:"));
                    for line in compile.lines() {
                        println!("    {}", style.dim(line));
                    }
                }
            }
            // Both verdicts must agree: every rendered case passed AND the
            // server's own overall flag — defensive against a truncated list.
            Ok(if all_passed && report.passed {
                CommandExit::Success
            } else {
                CommandExit::Failure
            })
        }
        Err(CloudError::NotPaired) => {
            println!(
                "{}",
                style.yellow(
                    "remote testing needs a paired device — run `promptly pair`, then \
                     `promptly test` again",
                ),
            );
            Ok(CommandExit::Failure)
        }
        Err(CloudError::UnsupportedEndpoint) => {
            let hint = local_hint
                .map(|rt| format!(" — install {rt} to run the tests locally"))
                .unwrap_or_default();
            println!(
                "{}",
                style.yellow(&format!(
                    "remote testing isn't available on this server yet{hint}"
                )),
            );
            Ok(CommandExit::Failure)
        }
        Err(err) => {
            println!("{} {err}", style.red("remote test failed:"));
            Ok(CommandExit::Failure)
        }
    }
}

/// Project the server's case verdicts onto the local renderer's shape. Statuses
/// map conservatively: anything that isn't a definite pass/fail (`errored`,
/// `missing`, or a status this CLI doesn't know yet) renders as an error line.
fn remote_case_results(report: &RemoteTestReport) -> Vec<CaseResult> {
    report
        .cases
        .iter()
        .map(|case| CaseResult {
            name: case.name.clone(),
            status: match case.status.as_str() {
                "passed" => CaseStatus::Passed,
                "failed" => CaseStatus::Failed,
                _ => CaseStatus::Errored,
            },
            detail: case
                .message
                .clone()
                .or_else(|| (case.status == "missing").then(|| "no verdict returned".to_string())),
        })
        .collect()
}

/// Find the public test file: prefer `tests/public/cases.json`, else the first
/// `tests/public/*.json` by name.
fn find_case_file(workspace: &Path) -> Option<PathBuf> {
    let dir = workspace.join(PUBLIC_TESTS_DIR);
    let canonical = dir.join("cases.json");
    if canonical.is_file() {
        return Some(canonical);
    }
    let mut jsons: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    jsons.sort();
    jsons.into_iter().next()
}

fn read_case_file(path: &Path) -> anyhow::Result<CaseFile> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Render the per-case lines and a summary, returning the text and whether every
/// case passed.
pub fn render_results(results: &[CaseResult], style: Style) -> (String, bool) {
    let mut out = String::new();
    let mut passed = 0usize;
    for result in results {
        let ok = result.status == CaseStatus::Passed;
        if ok {
            passed += 1;
        }
        let label = match result.status {
            CaseStatus::Passed => style.green("pass"),
            CaseStatus::Failed => style.red("fail"),
            CaseStatus::Errored => style.yellow("err "),
        };
        let detail = result
            .detail
            .as_deref()
            .map(|d| format!("  {}", style.dim(d)))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {} {} {}{}\n",
            style.mark(ok),
            label,
            result.name,
            detail,
        ));
    }
    let total = results.len();
    let all_passed = passed == total && total > 0;
    // The pass rate as a meter, colored by how close the suite is to green.
    let ratio = if total > 0 {
        passed as f64 / total as f64
    } else {
        0.0
    };
    let bar = visual::meter(ratio, 12);
    let bar = if all_passed {
        style.green(&bar)
    } else if passed > 0 {
        style.yellow(&bar)
    } else {
        style.red(&bar)
    };
    let summary = format!("{passed}/{total} passed");
    out.push_str(&format!(
        "  {} {}\n",
        bar,
        if all_passed {
            style.green(&summary)
        } else {
            style.bold(&summary)
        },
    ));
    (out, all_passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::RemoteCase;
    use crate::submission::SubmissionBundle;

    /// A workspace with a manifest and a public test file declaring `harness`,
    /// so the suite-harness branch deterministically routes to the remote path
    /// (independent of which toolchains the test machine has installed).
    fn temp_workspace(label: &str, harness: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("promptly-cmd-test-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".promptly")).unwrap();
        std::fs::create_dir_all(dir.join(PUBLIC_TESTS_DIR)).unwrap();
        std::fs::write(dir.join("lru.go"), "package main\n").unwrap();
        // The manifest's entry point must exist for packaging to validate.
        std::fs::write(dir.join("main.go"), "package main\n").unwrap();
        std::fs::write(
            dir.join(".promptly/manifest.json"),
            r#"{"schema_version":1,"level_id":"x","slug":"stage-1-01","baseline_hash":"y",
                "file_allowlist":["lru.go"],"entry_points":["main.go"]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(PUBLIC_TESTS_DIR).join("cases.json"),
            format!(r#"{{"harness":"{harness}","runtime_version":"go1.22","cases":[]}}"#),
        )
        .unwrap();
        dir
    }

    fn remote_report(
        cases: &[(&str, &str, Option<&str>)],
        passed: bool,
        crashed: bool,
        compile: Option<&str>,
    ) -> RemoteTestReport {
        RemoteTestReport {
            passed,
            crashed,
            cases: cases
                .iter()
                .map(|(name, status, message)| RemoteCase {
                    name: name.to_string(),
                    status: status.to_string(),
                    message: message.map(str::to_string),
                })
                .collect(),
            compile_output: compile.map(str::to_string),
        }
    }

    /// A scripted remote seam: returns its one response, asserting the CLI sent
    /// the manifest's slug and the allowlisted solution file.
    struct FakeRemote {
        response: std::cell::RefCell<Option<Result<RemoteTestReport, CloudError>>>,
    }

    impl FakeRemote {
        fn with(response: Result<RemoteTestReport, CloudError>) -> Self {
            Self {
                response: std::cell::RefCell::new(Some(response)),
            }
        }
    }

    impl RemoteTests for FakeRemote {
        fn run_public_tests(
            &self,
            slug: &str,
            bundle: &SubmissionBundle,
        ) -> Result<RemoteTestReport, CloudError> {
            assert_eq!(slug, "stage-1-01", "the manifest's slug is tested");
            assert!(
                bundle.files.iter().any(|f| f.path == "lru.go"),
                "the allowlisted solution is uploaded"
            );
            self.response.borrow_mut().take().expect("called once")
        }
    }

    /// A remote that must never be reached — guards the fail-early paths.
    struct NoRemote;
    impl RemoteTests for NoRemote {
        fn run_public_tests(
            &self,
            _slug: &str,
            _bundle: &SubmissionBundle,
        ) -> Result<RemoteTestReport, CloudError> {
            panic!("the remote endpoint must not be called")
        }
    }

    #[test]
    fn a_suite_harness_runs_remotely_and_reports_success() {
        let ws = temp_workspace("remote-pass", "multi_file");
        let manifest = Manifest::load(&ws).unwrap();
        let remote = FakeRemote::with(Ok(remote_report(
            &[("a", "passed", None), ("b", "passed", None)],
            true,
            false,
            None,
        )));
        let exit = run(&ws, Some(&manifest), &remote, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Success);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_failing_remote_suite_exits_nonzero() {
        let ws = temp_workspace("remote-fail", "multi_file");
        let manifest = Manifest::load(&ws).unwrap();
        let remote = FakeRemote::with(Ok(remote_report(
            &[
                ("a", "passed", None),
                ("b", "failed", Some("expected 3")),
                ("c", "errored", Some("compile error")),
                ("d", "missing", None),
            ],
            false,
            true,
            Some("main.go:3: undefined: Cache"),
        )));
        let exit = run(&ws, Some(&manifest), &remote, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn the_remote_fallback_requires_a_manifest() {
        // A public test file exists but the manifest doesn't: nothing names the
        // level or the allowlist, so fail with init guidance before any upload
        // (NoRemote panics if reached).
        let ws = temp_workspace("no-manifest", "multi_file");
        std::fs::remove_file(ws.join(".promptly/manifest.json")).unwrap();
        let exit = run(&ws, None, &NoRemote, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn an_unpaired_device_fails_cleanly() {
        let ws = temp_workspace("unpaired", "multi_file");
        let manifest = Manifest::load(&ws).unwrap();
        let remote = FakeRemote::with(Err(CloudError::NotPaired));
        let exit = run(&ws, Some(&manifest), &remote, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_server_without_the_endpoint_fails_cleanly() {
        let ws = temp_workspace("unsupported", "multi_file");
        let manifest = Manifest::load(&ws).unwrap();
        let remote = FakeRemote::with(Err(CloudError::UnsupportedEndpoint));
        let exit = run(&ws, Some(&manifest), &remote, Style::plain()).unwrap();
        assert_eq!(exit, CommandExit::Failure);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn remote_case_results_map_the_server_statuses() {
        let report = remote_report(
            &[
                ("a", "passed", None),
                ("b", "failed", Some("expected 3")),
                ("c", "errored", Some("compile error")),
                ("d", "missing", None),
                ("e", "some-future-status", None),
            ],
            false,
            false,
            None,
        );
        let results = remote_case_results(&report);
        assert_eq!(results[0].status, CaseStatus::Passed);
        assert_eq!(results[1].status, CaseStatus::Failed);
        assert_eq!(results[1].detail.as_deref(), Some("expected 3"));
        assert_eq!(results[2].status, CaseStatus::Errored);
        // `missing` degrades to an error line with a stand-in note…
        assert_eq!(results[3].status, CaseStatus::Errored);
        assert_eq!(results[3].detail.as_deref(), Some("no verdict returned"));
        // …and so does a status this CLI doesn't know yet.
        assert_eq!(results[4].status, CaseStatus::Errored);
        assert_eq!(results[4].detail, None);
    }

    fn result(name: &str, status: CaseStatus, detail: Option<&str>) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn render_marks_each_case_and_summarizes() {
        let results = vec![
            result("a", CaseStatus::Passed, None),
            result(
                "b",
                CaseStatus::Failed,
                Some("expected 3 byte(s) of output, got 1"),
            ),
            result("c", CaseStatus::Errored, Some("compile error")),
        ];
        let (text, all_passed) = render_results(&results, Style::plain());
        assert!(!all_passed);
        assert!(text.contains("pass a"));
        assert!(text.contains("fail b"));
        assert!(text.contains("err  c"));
        assert!(text.contains("1/3 passed"));
        // The pass-rate meter is partially filled: some fill, some track.
        assert!(text.contains('█'));
        assert!(text.contains('░'));
    }

    #[test]
    fn all_passing_reports_success() {
        let results = vec![
            result("a", CaseStatus::Passed, None),
            result("b", CaseStatus::Passed, None),
        ];
        let (text, all_passed) = render_results(&results, Style::plain());
        assert!(all_passed);
        assert!(text.contains("2/2 passed"));
        // A green suite fills the whole meter — no track left.
        assert!(text.contains("████████████"));
        assert!(!text.contains('░'));
    }

    #[test]
    fn an_empty_suite_is_not_a_pass() {
        let (_text, all_passed) = render_results(&[], Style::plain());
        assert!(!all_passed, "no cases must not report success");
    }

    #[test]
    fn finds_the_canonical_cases_file() {
        let dir = std::env::temp_dir().join(format!("promptly-test-find-{}", std::process::id()));
        let public = dir.join(PUBLIC_TESTS_DIR);
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(public.join("cases.json"), "{}").unwrap();
        std::fs::write(public.join("aaa.json"), "{}").unwrap();
        // `cases.json` wins even though `aaa.json` sorts first.
        assert_eq!(find_case_file(&dir), Some(public.join("cases.json")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
