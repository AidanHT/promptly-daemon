//! Filesystem locations and Claude Code's per-project log-folder naming rule.
//!
//! Two homes matter to the daemon: Promptly's own machine-local data dir
//! (`~/.promptly`, holding the checkpoint and lock files) and Claude Code's
//! (`~/.claude`, whose `projects/` subtree holds the JSONL session logs the
//! fallback watcher tails). Both default off the OS home dir but honor env
//! overrides so tests can redirect them at a tempdir.

use std::path::PathBuf;

/// Env var overriding the resolved OS home dir (used by both defaults below).
pub const HOME_ENV: &str = "PROMPTLY_HOME";
/// Env var overriding Promptly's data dir outright.
pub const DATA_DIR_ENV: &str = "PROMPTLY_DATA_DIR";
/// Env var overriding Claude Code's home dir outright.
pub const CLAUDE_HOME_ENV: &str = "PROMPTLY_CLAUDE_HOME";
/// Env var overriding Cursor's `User` dir (the `21` Cursor adapter's source).
pub const CURSOR_DIR_ENV: &str = "PROMPTLY_CURSOR_DIR";
/// Env var overriding the VS Code-family `User` dir (the `21` Copilot adapter).
pub const VSCODE_DIR_ENV: &str = "PROMPTLY_VSCODE_DIR";
/// Env var overriding OpenAI Codex CLI's sessions dir (the `21` Codex adapter).
pub const CODEX_DIR_ENV: &str = "PROMPTLY_CODEX_DIR";

fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os(HOME_ENV) {
        return PathBuf::from(dir);
    }
    // `dirs::home_dir` reads %USERPROFILE% on Windows and $HOME elsewhere.
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Promptly's machine-local data directory (`~/.promptly`). Holds the crash
/// checkpoint, the process/session lock files, and logs. Never synced.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    home().join(".promptly")
}

/// File name of the single-instance lock under [`data_dir`].
pub const PROCESS_LOCK_FILE: &str = "promptlyd.lock";

/// Full path of the single-`promptlyd`-per-machine lock
/// (`~/.promptly/promptlyd.lock`). Exposed so the CLI's daemon auto-management can
/// wait on the lock to confirm a stopped daemon has fully exited before
/// relaunching — the same path [`crate::config::DaemonConfig::process_lock_path`]
/// builds from the daemon's own captured data dir.
pub fn process_lock_path() -> PathBuf {
    data_dir().join(PROCESS_LOCK_FILE)
}

/// Claude Code's home directory (`~/.claude`).
pub fn claude_home() -> PathBuf {
    if let Some(dir) = std::env::var_os(CLAUDE_HOME_ENV) {
        return PathBuf::from(dir);
    }
    home().join(".claude")
}

/// The directory holding Claude Code's per-project session logs
/// (`~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`).
pub fn claude_projects_dir() -> PathBuf {
    claude_home().join("projects")
}

/// Cursor's `User` directory — `globalStorage/state.vscdb` (the bubble store)
/// and `workspaceStorage/<hash>/` (per-workspace state) live under it (`21`). On
/// Windows that's `%APPDATA%\Cursor\User`; macOS
/// `~/Library/Application Support/Cursor/User`; Linux `~/.config/Cursor/User`.
pub fn cursor_user_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CURSOR_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    dirs::config_dir().map(|c| c.join("Cursor").join("User"))
}

/// VS Code-family `User` directories to search for Copilot Chat sessions (stable
/// VS Code plus Insiders). When the override is set it replaces the list (`21`).
pub fn vscode_user_dirs() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os(VSCODE_DIR_ENV) {
        return vec![PathBuf::from(dir)];
    }
    match dirs::config_dir() {
        Some(config) => ["Code", "Code - Insiders"]
            .into_iter()
            .map(|app| config.join(app).join("User"))
            .collect(),
        None => Vec::new(),
    }
}

/// OpenAI Codex CLI's sessions directory (`~/.codex/sessions`), holding the
/// `YYYY/MM/DD/rollout-*.jsonl` transcripts (`21`).
pub fn codex_sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CODEX_DIR_ENV) {
        return PathBuf::from(dir);
    }
    home().join(".codex").join("sessions")
}

/// Strip Windows' extended-length prefix (`\\?\`, or `\\?\UNC\`) that
/// `std::fs::canonicalize` adds, so the path matches the plain cwd Claude Code
/// records (and encodes its project folder from). A no-op off Windows.
pub fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(s) = path.to_str() {
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// Encode an absolute workspace path the way Claude Code names its per-project
/// log folder under `~/.claude/projects/`: **every character that is not an
/// ASCII alphanumeric becomes `-`**. So `C:\Users\me\My Repo` becomes
/// `C--Users-me-My-Repo` and Unix `/work/promptly` becomes `-work-promptly`.
///
/// The transform is intentionally lossy — distinct paths collide (`a/b` and
/// `a-b` both encode to `a-b`) — so the JSONL watcher never trusts the folder
/// name alone; it disambiguates against the `cwd` field carried inside each
/// JSONL entry (`crate::sources::jsonl`).
pub fn encode_project_dir(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_windows_path_replacing_separators_colon_and_spaces() {
        // Ground truth: this repo's real `~/.claude/projects` folder name.
        assert_eq!(
            encode_project_dir(
                r"C:\Users\Quant\Documents\Programming\Projects\Full Stack\Promptly"
            ),
            "C--Users-Quant-Documents-Programming-Projects-Full-Stack-Promptly",
        );
    }

    #[test]
    fn encodes_unix_path_with_leading_separator() {
        assert_eq!(encode_project_dir("/work/promptly"), "-work-promptly");
        assert_eq!(encode_project_dir("/home/me/promptly"), "-home-me-promptly");
    }

    #[test]
    fn encodes_dots_and_is_lossy() {
        // Dots become `-` per the rule; and two different paths can collide.
        assert_eq!(encode_project_dir("a.b"), "a-b");
        assert_eq!(
            encode_project_dir("a/b"),
            encode_project_dir("a-b"),
            "encoding is lossy by design; callers disambiguate with the cwd field",
        );
    }

    #[cfg(windows)]
    #[test]
    fn strips_windows_extended_length_prefix() {
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\?\C:\work\promptly")),
            PathBuf::from(r"C:\work\promptly"),
        );
        // Plain paths are untouched.
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"C:\work\promptly")),
            PathBuf::from(r"C:\work\promptly"),
        );
    }

    #[test]
    fn process_lock_path_sits_in_the_data_dir() {
        // Component-wise `ends_with` (not the env-sensitive parent) keeps this from
        // racing the `DATA_DIR_ENV` override test that runs in parallel.
        assert!(process_lock_path().ends_with(PROCESS_LOCK_FILE));
    }

    #[test]
    fn env_overrides_redirect_homes() {
        // Process-global env: scope the override tightly and restore after.
        let prev = std::env::var_os(DATA_DIR_ENV);
        std::env::set_var(DATA_DIR_ENV, "/tmp/promptly-test");
        assert_eq!(data_dir(), PathBuf::from("/tmp/promptly-test"));
        match prev {
            Some(v) => std::env::set_var(DATA_DIR_ENV, v),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
    }

    #[test]
    fn adapter_source_overrides_redirect() {
        let prev_cursor = std::env::var_os(CURSOR_DIR_ENV);
        let prev_vscode = std::env::var_os(VSCODE_DIR_ENV);
        let prev_codex = std::env::var_os(CODEX_DIR_ENV);

        std::env::set_var(CURSOR_DIR_ENV, "/tmp/cur/User");
        std::env::set_var(VSCODE_DIR_ENV, "/tmp/code/User");
        std::env::set_var(CODEX_DIR_ENV, "/tmp/codex/sessions");
        assert_eq!(cursor_user_dir(), Some(PathBuf::from("/tmp/cur/User")));
        assert_eq!(vscode_user_dirs(), vec![PathBuf::from("/tmp/code/User")]);
        assert_eq!(codex_sessions_dir(), PathBuf::from("/tmp/codex/sessions"));

        let restore = |key: &str, prev: Option<std::ffi::OsString>| match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        };
        restore(CURSOR_DIR_ENV, prev_cursor);
        restore(VSCODE_DIR_ENV, prev_vscode);
        restore(CODEX_DIR_ENV, prev_codex);
    }
}
