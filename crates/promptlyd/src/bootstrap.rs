//! Consented Claude Code harness bootstrap (`18`).
//!
//! To capture via OpenTelemetry, Claude Code must export to the daemon's local
//! receiver. The daemon writes the needed env into the **project-level** Claude
//! settings (`<workspace>/.claude/settings.json`) — never the user's global
//! config — so capture "just works" inside a Promptly workspace.
//!
//! This is purely the *mechanism*: the consent decision and the consent record
//! live in the session lifecycle (`crate::scoping`). Every change is reversible —
//! [`apply`] captures the exact prior state and [`revert`] restores it — and
//! idempotent: [`reapply`] re-asserts the env on resume without disturbing the
//! captured prior, so re-running `start` never duplicates or corrupts settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Project Claude settings directory, relative to the workspace root.
pub const CLAUDE_DIR: &str = ".claude";
/// Project Claude settings file within [`CLAUDE_DIR`].
pub const SETTINGS_FILE: &str = "settings.json";

/// The project settings path for a workspace.
pub fn settings_path(workspace: &Path) -> PathBuf {
    workspace.join(CLAUDE_DIR).join(SETTINGS_FILE)
}

/// The settings key carrying the per-session OTLP ingest token. Claude Code's OTEL
/// exporter forwards `OTEL_EXPORTER_OTLP_HEADERS` verbatim as request headers, so
/// the daemon authenticates ingest by minting a fresh token per consented session,
/// writing it here, and requiring it at the receiver ([`crate::sources::otel`]).
/// Without it any loopback process could POST fabricated `api_request` events into
/// the capture stream and inflate — or forge an entire — verified attempt.
pub const OTLP_TOKEN_ENV_KEY: &str = "OTEL_EXPORTER_OTLP_HEADERS";
/// The header name (inside [`OTLP_TOKEN_ENV_KEY`]) the receiver checks. A custom
/// header avoids the percent-encoding pitfalls of stuffing a bearer into standard
/// auth env, and OTEL's `key=value` header syntax carries it unambiguously.
pub const OTLP_TOKEN_HEADER: &str = "X-Promptly-Otlp-Token";

/// The env Claude Code needs to export telemetry to the daemon's loopback OTLP
/// receiver. The receiver speaks **OTLP/HTTP+JSON** (returns `415` for protobuf),
/// hence `http/json`; the turn unit is the `api_request` **log** event, so the
/// logs exporter is the load-bearing one (metrics is set for completeness). When
/// `otlp_token` is present it is written as the ingest-auth header so the receiver
/// can reject unauthenticated posts; the preview path passes `None` (the token is
/// minted at session start, so its value isn't known until then).
pub fn desired_env(otlp_endpoint: &str, otlp_token: Option<&str>) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1".to_string()),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", otlp_endpoint.to_string()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json".to_string()),
        ("OTEL_LOGS_EXPORTER", "otlp".to_string()),
        ("OTEL_METRICS_EXPORTER", "otlp".to_string()),
    ];
    if let Some(token) = otlp_token {
        env.push((OTLP_TOKEN_ENV_KEY, format!("{OTLP_TOKEN_HEADER}={token}")));
    }
    env
}

/// One env key's value before the daemon touched it: `None` means the key didn't
/// exist, so revert removes it; `Some` means restore that exact prior value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorEntry {
    pub key: String,
    pub value: Option<Value>,
}

/// Everything needed to undo a bootstrap exactly, persisted in the session marker
/// so a later `stop` (even after a daemon restart) can restore the player's
/// settings to the byte.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapState {
    /// Whether `settings.json` existed before bootstrap (if not, revert deletes it).
    pub file_existed: bool,
    /// Whether the `.claude/` directory existed before bootstrap. Revert removes it
    /// (when empty) only if the daemon created it; if the player already had it,
    /// revert leaves it alone. `#[serde(default)]` is `true` so a marker written
    /// before this field is conservative on upgrade — assume the dir was the
    /// player's and never delete it.
    #[serde(default = "default_true")]
    pub dir_existed: bool,
    /// Whether an `env` object existed before (if not, revert drops an emptied one).
    pub env_existed: bool,
    /// Prior value of each key the daemon set.
    pub prior: Vec<PriorEntry>,
}

fn default_true() -> bool {
    true
}

/// A non-destructive description of what bootstrapping would change, for the
/// consent prompt that names the exact file and keys (`19` renders it).
#[derive(Debug, Clone)]
pub struct BootstrapPlan {
    pub settings_path: PathBuf,
    pub file_exists: bool,
    /// The env keys that would be written.
    pub keys: Vec<&'static str>,
    pub endpoint: String,
    /// True when the file already carries exactly these values (a no-op).
    pub already_applied: bool,
}

/// Read `settings.json` as a JSON object: `{}` when absent, an error when present
/// but malformed (never silently clobber a player's settings) or not an object.
fn read_root(path: &Path) -> std::io::Result<Map<String, Value>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.iter().all(u8::is_ascii_whitespace) {
                return Ok(Map::new());
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} is not valid JSON: {e}", path.display()),
                )
            })?;
            match value {
                Value::Object(map) => Ok(map),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} is not a JSON object", path.display()),
                )),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(err) => Err(err),
    }
}

fn write_root(path: &Path, root: &Map<String, Value>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root.clone()))?;
    text.push('\n');
    std::fs::write(path, text)
}

fn env_object(root: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let entry = root.entry("env").or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().expect("env is an object")
}

/// Compute (without writing) what bootstrap would do, for the consent prompt.
pub fn plan(workspace: &Path, otlp_endpoint: &str) -> std::io::Result<BootstrapPlan> {
    let path = settings_path(workspace);
    let file_exists = path.exists();
    // The stable keys whose values are known at preview time (the per-session token
    // isn't minted until start, so it's previewed by name only, below).
    let desired = desired_env(otlp_endpoint, None);
    let already_applied = if file_exists {
        let root = read_root(&path)?;
        let env = root.get("env").and_then(Value::as_object);
        desired
            .iter()
            .all(|(k, v)| env.and_then(|e| e.get(*k)).and_then(Value::as_str) == Some(v.as_str()))
    } else {
        false
    };
    let mut keys: Vec<&'static str> = desired.iter().map(|(k, _)| *k).collect();
    // Name the ingest-token header the consented start will also write, so the
    // consent prompt is honest about every key the bootstrap touches.
    keys.push(OTLP_TOKEN_ENV_KEY);
    Ok(BootstrapPlan {
        settings_path: path,
        file_exists,
        keys,
        endpoint: otlp_endpoint.to_string(),
        already_applied,
    })
}

/// Inject the OTEL env, capturing the exact prior state for a clean revert. Call
/// once per session (the lifecycle persists the returned state); use [`reapply`]
/// to re-assert it on resume.
pub fn apply(
    workspace: &Path,
    otlp_endpoint: &str,
    otlp_token: &str,
) -> std::io::Result<BootstrapState> {
    let path = settings_path(workspace);
    let file_existed = path.exists();
    // Capture whether `.claude/` already exists *before* write_root creates it,
    // so revert only removes a directory the daemon itself created.
    let dir_existed = path.parent().is_some_and(Path::exists);
    let mut root = read_root(&path)?;
    let env_existed = root.get("env").map(Value::is_object).unwrap_or(false);

    let env = env_object(&mut root);
    let mut prior = Vec::new();
    for (key, value) in desired_env(otlp_endpoint, Some(otlp_token)) {
        prior.push(PriorEntry {
            key: key.to_string(),
            value: env.get(key).cloned(),
        });
        env.insert(key.to_string(), Value::String(value));
    }
    write_root(&path, &root)?;
    Ok(BootstrapState {
        file_existed,
        dir_existed,
        env_existed,
        prior,
    })
}

/// Re-assert the desired env without recapturing prior state — idempotent, for
/// resuming a session whose [`BootstrapState`] is already recorded.
pub fn reapply(
    workspace: &Path,
    otlp_endpoint: &str,
    otlp_token: Option<&str>,
) -> std::io::Result<()> {
    let path = settings_path(workspace);
    let mut root = read_root(&path)?;
    let env = env_object(&mut root);
    for (key, value) in desired_env(otlp_endpoint, otlp_token) {
        env.insert(key.to_string(), Value::String(value));
    }
    write_root(&path, &root)
}

/// Restore the settings to their pre-bootstrap state: put back each key's prior
/// value (or remove keys that didn't exist), drop an `env` object the daemon
/// created, and delete `settings.json` (and an empty `.claude/`) if bootstrap
/// created it.
pub fn revert(workspace: &Path, state: &BootstrapState) -> std::io::Result<()> {
    let path = settings_path(workspace);
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_root(&path)?;
    if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
        for entry in &state.prior {
            match &entry.value {
                Some(value) => {
                    env.insert(entry.key.clone(), value.clone());
                }
                None => {
                    env.remove(&entry.key);
                }
            }
        }
        if env.is_empty() && !state.env_existed {
            root.remove("env");
        }
    }

    if !state.file_existed && root.is_empty() {
        std::fs::remove_file(&path)?;
        // Best-effort: drop `.claude/` only if the daemon created it (it didn't
        // exist before bootstrap) and it's now empty — never a dir the player owned.
        if !state.dir_existed {
            let _ = std::fs::remove_dir(path.parent().unwrap_or(&path));
        }
        return Ok(());
    }
    write_root(&path, &root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDPOINT: &str = "http://127.0.0.1:4318";

    fn temp_ws(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "promptlyd-bootstrap-{}-{label}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_env(workspace: &Path) -> Map<String, Value> {
        let root = read_root(&settings_path(workspace)).unwrap();
        root.get("env")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn desired_env_points_at_the_loopback_receiver_via_http_json() {
        let env = desired_env(ENDPOINT, None);
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map["CLAUDE_CODE_ENABLE_TELEMETRY"], "1");
        assert_eq!(map["OTEL_EXPORTER_OTLP_ENDPOINT"], ENDPOINT);
        assert_eq!(map["OTEL_EXPORTER_OTLP_PROTOCOL"], "http/json");
        assert_eq!(map["OTEL_LOGS_EXPORTER"], "otlp");
        // Without a token the ingest-auth header is absent (the preview path).
        assert!(!map.contains_key(OTLP_TOKEN_ENV_KEY));
    }

    #[test]
    fn desired_env_carries_the_ingest_token_header_when_present() {
        let env = desired_env(ENDPOINT, Some("deadbeef"));
        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        // The token rides inside OTEL_EXPORTER_OTLP_HEADERS as `Header=value`, which
        // Claude Code's exporter forwards verbatim to the receiver.
        assert_eq!(
            map[OTLP_TOKEN_ENV_KEY],
            "X-Promptly-Otlp-Token=deadbeef".to_string()
        );
    }

    #[test]
    fn apply_writes_the_ingest_token_header_and_revert_removes_it() {
        let ws = temp_ws("token");
        let state = apply(&ws, ENDPOINT, "tok-12345").unwrap();
        let env = read_env(&ws);
        assert_eq!(
            env[OTLP_TOKEN_ENV_KEY],
            json!("X-Promptly-Otlp-Token=tok-12345")
        );
        // The consent preview names the token header among the keys it will write.
        assert!(plan(&ws, ENDPOINT)
            .unwrap()
            .keys
            .contains(&OTLP_TOKEN_ENV_KEY));

        revert(&ws, &state).unwrap();
        assert!(
            !settings_path(&ws).exists(),
            "the daemon-created file is gone"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn apply_into_a_fresh_workspace_writes_project_settings_then_revert_removes_them() {
        let ws = temp_ws("fresh");
        let preview = plan(&ws, ENDPOINT).unwrap();
        assert!(!preview.file_exists && !preview.already_applied);
        assert!(preview.settings_path.ends_with("settings.json"));

        let state = apply(&ws, ENDPOINT, "tok").unwrap();
        let env = read_env(&ws);
        assert_eq!(env["OTEL_EXPORTER_OTLP_ENDPOINT"], json!(ENDPOINT));
        assert!(plan(&ws, ENDPOINT).unwrap().already_applied);

        revert(&ws, &state).unwrap();
        // The file the daemon created is gone, restoring the workspace exactly.
        assert!(!settings_path(&ws).exists());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn revert_keeps_a_dot_claude_dir_the_player_already_had() {
        let ws = temp_ws("preexisting-dir");
        // The player already has an (empty) .claude/ directory but no settings.json.
        let claude_dir = settings_path(&ws).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&claude_dir).unwrap();
        assert!(!settings_path(&ws).exists());

        let state = apply(&ws, ENDPOINT, "tok").unwrap();
        assert!(!state.file_existed);
        assert!(state.dir_existed, ".claude/ existed before bootstrap");
        assert!(settings_path(&ws).exists());

        revert(&ws, &state).unwrap();
        // The daemon-created settings.json is gone, but the player's own .claude/
        // directory must survive — revert only deletes what the daemon created.
        assert!(!settings_path(&ws).exists());
        assert!(
            claude_dir.exists(),
            "a .claude/ directory the player owned must not be deleted"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn apply_preserves_unrelated_settings_and_prior_env_then_revert_restores_them() {
        let ws = temp_ws("preserve");
        // The player already has settings, including their own OTEL endpoint.
        let existing = json!({
            "model": "claude-opus-4-8",
            "env": { "OTEL_EXPORTER_OTLP_ENDPOINT": "http://example.test", "FOO": "bar" }
        });
        std::fs::create_dir_all(settings_path(&ws).parent().unwrap()).unwrap();
        std::fs::write(
            settings_path(&ws),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let state = apply(&ws, ENDPOINT, "tok").unwrap();
        let env = read_env(&ws);
        // Our endpoint wins while capturing; unrelated keys are untouched.
        assert_eq!(env["OTEL_EXPORTER_OTLP_ENDPOINT"], json!(ENDPOINT));
        assert_eq!(env["FOO"], json!("bar"));

        revert(&ws, &state).unwrap();
        let root = read_root(&settings_path(&ws)).unwrap();
        let env = root.get("env").and_then(Value::as_object).unwrap();
        // The player's original endpoint and unrelated settings are restored.
        assert_eq!(
            env["OTEL_EXPORTER_OTLP_ENDPOINT"],
            json!("http://example.test")
        );
        assert_eq!(env["FOO"], json!("bar"));
        assert!(!env.contains_key("CLAUDE_CODE_ENABLE_TELEMETRY"));
        assert_eq!(root["model"], json!("claude-opus-4-8"));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn reapply_is_idempotent_and_does_not_duplicate() {
        let ws = temp_ws("idempotent");
        apply(&ws, ENDPOINT, "tok").unwrap();
        reapply(&ws, ENDPOINT, Some("tok")).unwrap();
        reapply(&ws, ENDPOINT, Some("tok")).unwrap();
        let env = read_env(&ws);
        // Exactly the desired keys (including the token header), each once.
        for (key, value) in desired_env(ENDPOINT, Some("tok")) {
            assert_eq!(env[key], json!(value));
        }
        assert_eq!(env.len(), desired_env(ENDPOINT, Some("tok")).len());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn malformed_settings_are_refused_rather_than_clobbered() {
        let ws = temp_ws("malformed");
        std::fs::create_dir_all(settings_path(&ws).parent().unwrap()).unwrap();
        std::fs::write(settings_path(&ws), "{ not json").unwrap();
        assert!(apply(&ws, ENDPOINT, "tok").is_err());
        // The player's file is left exactly as it was.
        assert_eq!(
            std::fs::read_to_string(settings_path(&ws)).unwrap(),
            "{ not json"
        );
        std::fs::remove_dir_all(&ws).ok();
    }
}
