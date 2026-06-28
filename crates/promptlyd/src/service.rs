//! OS service installation.
//!
//! `promptlyd install` registers the daemon as a managed background service so it
//! runs at login: a systemd **user** service on Linux, a launchd **agent** on
//! macOS, and a logon **scheduled task** on Windows. `uninstall` reverses it.
//! Running in the foreground (`promptlyd run`) is always available for debugging.
//!
//! The installed service launches `promptlyd run` with the [`ServiceArgs`]
//! captured at install time (the workspace, ports, and any extra web origins), so
//! a background daemon scopes to the player's project instead of the service
//! manager's cwd. The unit/plist/argument **generators are pure and
//! unit-tested**; the apply step (which writes files and shells out to the
//! platform manager) is the thin OS-specific shell over them.

use std::path::{Path, PathBuf};

/// Service/task name and launchd label.
pub const SERVICE_NAME: &str = "promptlyd";
pub const LAUNCHD_LABEL: &str = "com.promptly.promptlyd";

/// The `run` arguments an installed service launches the daemon with. Captured at
/// install time (`install`) so the background daemon scopes to the right
/// workspace and ports rather than inheriting the service manager's cwd and bare
/// defaults. The canonical production web origin is added by `run` itself
/// (`config::resolve_web_origins`), so only *extra* `--web-origin`s are carried
/// here.
#[derive(Debug, Default, Clone)]
pub struct ServiceArgs {
    /// Absolute workspace to capture; `None` leaves it to the daemon's cwd default.
    pub workspace: Option<PathBuf>,
    /// Status/stream API port override; `None` uses the daemon default.
    pub api_port: Option<u16>,
    /// OTLP receiver port override; `None` uses the daemon default.
    pub otlp_port: Option<u16>,
    /// Additional deployed web origins beyond the baked-in production default.
    pub web_origins: Vec<String>,
}

impl ServiceArgs {
    /// The argv after the binary path: `run [--workspace …] [--api-port …] …`.
    /// Only set fields are emitted, so the common install is just `run
    /// --workspace "<cwd>"`.
    pub fn to_argv(&self) -> Vec<String> {
        let mut argv = vec!["run".to_string()];
        if let Some(workspace) = &self.workspace {
            argv.push("--workspace".to_string());
            argv.push(workspace.display().to_string());
        }
        if let Some(port) = self.api_port {
            argv.push("--api-port".to_string());
            argv.push(port.to_string());
        }
        if let Some(port) = self.otlp_port {
            argv.push("--otlp-port".to_string());
            argv.push(port.to_string());
        }
        for origin in &self.web_origins {
            argv.push("--web-origin".to_string());
            argv.push(origin.clone());
        }
        argv
    }
}

/// Render `exe` + argv into one command string for systemd's `ExecStart` and
/// Windows' `schtasks /TR`: the exe is always quoted and any token containing a
/// space (a workspace path) is double-quoted, which both parsers honor.
fn quoted_command(exe: &Path, argv: &[String]) -> String {
    let mut out = format!("\"{}\"", exe.display());
    for token in argv {
        if token.contains(' ') {
            out.push_str(&format!(" \"{token}\""));
        } else {
            out.push(' ');
            out.push_str(token);
        }
    }
    out
}

/// Minimal XML escaping for a launchd plist `<string>` (paths/origins can contain
/// `&`); the five predefined entities cover every case a path or origin produces.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// A systemd user-service unit that runs `promptlyd run` (with `args`) and
/// restarts on failure.
pub fn systemd_unit(exe: &Path, args: &ServiceArgs) -> String {
    format!(
        "[Unit]\n\
         Description=Promptly local telemetry daemon\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec = quoted_command(exe, &args.to_argv()),
    )
}

/// A launchd agent plist that runs `promptlyd run` (with `args`) at load and
/// keeps it alive. Each argv token is its own `<string>`, so paths with spaces
/// need no quoting.
pub fn launchd_plist(exe: &Path, args: &ServiceArgs) -> String {
    let mut elements = format!(
        "<string>{}</string>",
        xml_escape(&exe.display().to_string())
    );
    for token in args.to_argv() {
        elements.push_str(&format!("<string>{}</string>", xml_escape(&token)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>{elements}</array>\n\
         \t<key>RunAtLoad</key><true/>\n\
         \t<key>KeepAlive</key><true/>\n\
         </dict>\n\
         </plist>\n",
    )
}

/// `schtasks` arguments to create a logon task running `promptlyd run` (with
/// `args`).
pub fn schtasks_create_args(exe: &Path, args: &ServiceArgs) -> Vec<String> {
    vec![
        "/Create".into(),
        "/SC".into(),
        "ONLOGON".into(),
        "/TN".into(),
        SERVICE_NAME.into(),
        "/TR".into(),
        quoted_command(exe, &args.to_argv()),
        "/F".into(),
    ]
}

/// `schtasks` arguments to delete the logon task.
pub fn schtasks_delete_args() -> Vec<String> {
    vec![
        "/Delete".into(),
        "/TN".into(),
        SERVICE_NAME.into(),
        "/F".into(),
    ]
}

/// Install the daemon as a managed background service for the current OS,
/// launching it with `args`.
pub fn install(args: ServiceArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    install_os(&exe, &args)
}

/// Remove the managed background service.
pub fn uninstall() -> anyhow::Result<()> {
    uninstall_os()
}

#[cfg(target_os = "linux")]
fn install_os(exe: &Path, args: &ServiceArgs) -> anyhow::Result<()> {
    let unit_path = systemd_unit_path()?;
    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&unit_path, systemd_unit(exe, args))?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run(
        "systemctl",
        &["--user", "enable", "--now", "promptlyd.service"],
    )?;
    tracing::info!(unit = %unit_path.display(), "installed systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_os() -> anyhow::Result<()> {
    let _ = run(
        "systemctl",
        &["--user", "disable", "--now", "promptlyd.service"],
    );
    if let Ok(path) = systemd_unit_path() {
        let _ = std::fs::remove_file(path);
    }
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> anyhow::Result<std::path::PathBuf> {
    let config = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    Ok(config.join("systemd/user/promptlyd.service"))
}

#[cfg(target_os = "macos")]
fn install_os(exe: &Path, args: &ServiceArgs) -> anyhow::Result<()> {
    let plist_path = launchd_plist_path()?;
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plist_path, launchd_plist(exe, args))?;
    run("launchctl", &["load", &plist_path.to_string_lossy()])?;
    tracing::info!(plist = %plist_path.display(), "installed launchd agent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_os() -> anyhow::Result<()> {
    if let Ok(path) = launchd_plist_path() {
        let _ = run("launchctl", &["unload", &path.to_string_lossy()]);
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> anyhow::Result<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    Ok(home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
}

#[cfg(target_os = "windows")]
fn install_os(exe: &Path, args: &ServiceArgs) -> anyhow::Result<()> {
    let create = schtasks_create_args(exe, args);
    run(
        "schtasks",
        &create.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    tracing::info!(task = SERVICE_NAME, "installed logon scheduled task");
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_os() -> anyhow::Result<()> {
    let args = schtasks_delete_args();
    let _ = run(
        "schtasks",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn install_os(_exe: &Path, _args: &ServiceArgs) -> anyhow::Result<()> {
    anyhow::bail!("service installation is not supported on this OS; run `promptlyd run` instead")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn uninstall_os() -> anyhow::Result<()> {
    anyhow::bail!("service installation is not supported on this OS")
}

/// Run a platform manager command, failing on a non-zero exit.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new(program).args(args).status()?;
    if !status.success() {
        anyhow::bail!("`{program}` exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with_workspace(workspace: &str) -> ServiceArgs {
        ServiceArgs {
            workspace: Some(PathBuf::from(workspace)),
            ..Default::default()
        }
    }

    #[test]
    fn empty_args_run_the_bare_binary() {
        assert_eq!(ServiceArgs::default().to_argv(), vec!["run".to_string()]);
    }

    #[test]
    fn argv_emits_only_the_set_fields_in_order() {
        let args = ServiceArgs {
            workspace: Some(PathBuf::from("/work/proj")),
            api_port: Some(9000),
            otlp_port: None,
            web_origins: vec!["https://custom.example".into()],
        };
        assert_eq!(
            args.to_argv(),
            vec![
                "run",
                "--workspace",
                "/work/proj",
                "--api-port",
                "9000",
                "--web-origin",
                "https://custom.example",
            ]
        );
    }

    #[test]
    fn systemd_unit_runs_the_binary_with_its_args() {
        let unit = systemd_unit(
            &PathBuf::from("/usr/local/bin/promptlyd"),
            &args_with_workspace("/work/proj"),
        );
        assert!(unit.contains("ExecStart=\"/usr/local/bin/promptlyd\" run --workspace /work/proj"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn systemd_unit_quotes_a_workspace_with_spaces() {
        let unit = systemd_unit(
            &PathBuf::from("/usr/local/bin/promptlyd"),
            &args_with_workspace("/work/my proj"),
        );
        // The path with a space must be quoted so systemd parses one token.
        assert!(
            unit.contains("run --workspace \"/work/my proj\""),
            "got: {unit}"
        );
    }

    #[test]
    fn launchd_plist_has_label_and_each_arg_as_a_string() {
        let plist = launchd_plist(
            &PathBuf::from("/opt/promptlyd"),
            &args_with_workspace("/work/proj"),
        );
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        assert!(plist.contains(
            "<string>/opt/promptlyd</string><string>run</string>\
             <string>--workspace</string><string>/work/proj</string>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn schtasks_args_embed_the_run_command_and_delete_by_name() {
        let create = schtasks_create_args(
            &PathBuf::from(r"C:\Program Files\promptlyd.exe"),
            &args_with_workspace(r"C:\my proj"),
        );
        assert_eq!(create[0], "/Create");
        assert!(create.contains(&SERVICE_NAME.to_string()));
        // /TR carries the quoted exe + run + quoted workspace as one argument.
        let tr = &create[create.iter().position(|a| a == "/TR").unwrap() + 1];
        assert_eq!(
            tr,
            r#""C:\Program Files\promptlyd.exe" run --workspace "C:\my proj""#
        );

        let delete = schtasks_delete_args();
        assert_eq!(delete[0], "/Delete");
        assert!(delete.contains(&SERVICE_NAME.to_string()));
    }
}
