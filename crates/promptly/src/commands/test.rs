//! `promptly test` — run the level's public tests, preferring a fast local run
//! and falling back to the remote Judge0 service when local execution isn't
//! possible (`19`). Local results match the web staging pass/fail (`09`).

use std::path::{Path, PathBuf};

use crate::runner::{self, CaseFile, CaseResult, CaseStatus, LocalRuntime};
use crate::style::Style;
use crate::web_client::{WebClient, WebError};
use crate::CommandExit;

/// Conventional location of the public test set inside a workspace (`07`).
const PUBLIC_TESTS_DIR: &str = "tests/public";

pub fn run(workspace: &Path, web: &WebClient, style: Style) -> anyhow::Result<CommandExit> {
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
        return fallback(
            web,
            style,
            &format!("the '{}' harness runs server-side", file.harness),
        );
    }

    let runtime = match LocalRuntime::from_runtime_version(&file.runtime_version) {
        Some(rt) => rt,
        None => {
            return fallback(
                web,
                style,
                &format!(
                    "local execution isn't supported for {} yet",
                    file.runtime_version
                ),
            );
        }
    };
    let Some(program) = runtime.resolve_program() else {
        return fallback(
            web,
            style,
            &format!(
                "the {} toolchain isn't installed on PATH",
                file.runtime_version
            ),
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

/// Local execution isn't possible — explain why and route to the remote path,
/// reporting whether the Judge0 backend is reachable. The authenticated remote
/// grading itself arrives with cloud pairing (`20`).
fn fallback(web: &WebClient, style: Style, reason: &str) -> anyhow::Result<CommandExit> {
    println!("{} {reason}", style.yellow("local run unavailable —"));
    match web.execution_health() {
        Ok(health) if health.healthy => {
            println!(
                "  {}",
                style.dim(
                    "remote grading via Judge0 is available, but needs a paired device — \
                     run `promptly login` once cloud pairing ships (subplan 20)",
                ),
            );
        }
        Ok(health) => {
            println!(
                "  {}",
                style.yellow(&format!(
                    "remote Judge0 is currently unavailable ({})",
                    health.reason
                )),
            );
        }
        Err(WebError::NotReachable(base)) => {
            println!(
                "  {}",
                style.dim(&format!(
                    "couldn't reach the web app at {base} to check remote grading (set --api-url)"
                )),
            );
        }
        Err(err) => println!("  {}", style.dim(&format!("remote check failed: {err}"))),
    }
    Ok(CommandExit::Failure)
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
    let summary = format!("{passed}/{total} passed");
    out.push_str(&format!(
        "{}\n",
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
