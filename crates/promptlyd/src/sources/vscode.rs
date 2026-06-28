//! Shared VS Code-family storage helpers (`21`).
//!
//! Cursor and Copilot both build on VS Code's storage layout: per-workspace state
//! lives under `…/User/workspaceStorage/<hash>/`, and which filesystem folder a
//! `<hash>` belongs to is recorded in that dir's `workspace.json` as a `file://`
//! URI. To scope capture to the bound workspace, both adapters resolve the
//! `<hash>` dir whose `workspace.json` folder matches the workspace path — the
//! "map each tool's notion of a project to the bound workspace" the plan calls
//! for. The URI decoding and the folder match are pure and unit-tested here.

use std::path::{Path, PathBuf};

use super::jsonl::normalize_for_compare;

/// Decode a `file://` URI from a `workspace.json` into a filesystem path.
///
/// Handles the URL-encoding VS Code writes (`%3A` → `:`, `%20` → space) and the
/// Windows drive-letter form (`file:///c%3A/Users/me` → `c:\Users\me`). A value
/// that isn't a `file://` URI is treated as a literal path (defensive).
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let Some(after) = uri.strip_prefix("file://") else {
        return Some(PathBuf::from(uri));
    };
    let decoded = percent_decode(after);
    // After the (empty localhost) authority the path starts with `/`. On Windows
    // a drive path is `/C:/…`; drop that leading slash so it reads `C:/…`.
    let cleaned = match decoded.strip_prefix('/') {
        Some(rest) if is_drive_prefixed(rest) => rest.to_string(),
        _ => decoded,
    };
    if cleaned.is_empty() {
        None
    } else {
        Some(PathBuf::from(cleaned))
    }
}

/// Does `s` begin with a Windows drive letter (`C:` / `c:`)?
fn is_drive_prefixed(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Percent-decode `%XX` escapes; malformed escapes are left verbatim.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Extract the single-folder `folder` URI from a `workspace.json` and decode it.
/// Multi-root workspaces (which use a `workspace` key) return `None` — they have
/// no single bound folder to scope to.
pub fn parse_workspace_folder(workspace_json: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(workspace_json).ok()?;
    let folder = v.get("folder").and_then(serde_json::Value::as_str)?;
    file_uri_to_path(folder)
}

/// Whether a `workspace.json` folder denotes `target` (separator/case-insensitive
/// per [`normalize_for_compare`]).
pub fn folder_is(workspace_json: &str, target_norm: &str) -> bool {
    parse_workspace_folder(workspace_json)
        .map(|p| normalize_for_compare(&p.to_string_lossy()) == target_norm)
        .unwrap_or(false)
}

/// Find the `workspaceStorage/<hash>/` directory bound to `target`, by reading
/// each candidate's `workspace.json`. `None` if the root is absent or no entry
/// matches (the workspace was never opened in this editor).
pub fn find_workspace_storage(ws_storage_root: &Path, target: &Path) -> Option<PathBuf> {
    let target_norm = normalize_for_compare(&target.to_string_lossy());
    for entry in std::fs::read_dir(ws_storage_root).ok()?.flatten() {
        let dir = entry.path();
        match std::fs::read_to_string(dir.join("workspace.json")) {
            Ok(text) if folder_is(&text, &target_norm) => return Some(dir),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_unix_file_uri() {
        assert_eq!(
            file_uri_to_path("file:///home/me/My%20Repo"),
            Some(PathBuf::from("/home/me/My Repo")),
        );
    }

    #[test]
    fn decodes_windows_drive_file_uri() {
        // Encoded colon + space, with the drive-letter leading slash removed.
        assert_eq!(
            file_uri_to_path("file:///c%3A/Users/me/My%20Repo"),
            Some(PathBuf::from("c:/Users/me/My Repo")),
        );
        // Already-unencoded colon is handled too.
        assert_eq!(
            file_uri_to_path("file:///C:/work/promptly"),
            Some(PathBuf::from("C:/work/promptly")),
        );
    }

    #[test]
    fn non_uri_value_is_a_literal_path_and_empty_is_none() {
        assert_eq!(
            file_uri_to_path("/already/a/path"),
            Some(PathBuf::from("/already/a/path")),
        );
        assert_eq!(file_uri_to_path("file://"), None);
    }

    #[test]
    fn parses_single_folder_workspace_only() {
        let single = r#"{"folder":"file:///home/me/repo"}"#;
        assert_eq!(
            parse_workspace_folder(single),
            Some(PathBuf::from("/home/me/repo")),
        );
        // Multi-root (a `workspace` key) and malformed JSON resolve to no folder.
        assert_eq!(
            parse_workspace_folder(r#"{"workspace":"file:///x/team.code-workspace"}"#),
            None,
        );
        assert_eq!(parse_workspace_folder("{not json"), None);
    }

    #[test]
    fn finds_the_hash_dir_matching_the_workspace() {
        let base = std::env::temp_dir().join(format!("promptlyd-vscode-{}", std::process::id()));
        let root = base.join("workspaceStorage");
        let want = root.join("aaaa1111");
        let other = root.join("bbbb2222");
        std::fs::create_dir_all(&want).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            want.join("workspace.json"),
            r#"{"folder":"file:///work/promptly"}"#,
        )
        .unwrap();
        std::fs::write(
            other.join("workspace.json"),
            r#"{"folder":"file:///work/elsewhere"}"#,
        )
        .unwrap();

        let found = find_workspace_storage(&root, &PathBuf::from("/work/promptly"));
        assert_eq!(found.as_deref(), Some(want.as_path()));
        // A workspace never opened here isn't found.
        assert!(find_workspace_storage(&root, &PathBuf::from("/work/missing")).is_none());

        std::fs::remove_dir_all(&base).ok();
    }
}
