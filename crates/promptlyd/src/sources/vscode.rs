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

/// Are two normalized paths ([`normalize_for_compare`]) the same folder, or in
/// an ancestor/descendant relationship? The editors record the *project root*
/// they were opened in, which is routinely a **parent** of the bound level
/// workspace (a player opens the whole challenge folder, then plays a level
/// subfolder) — an exact-equality match silently captures nothing in that
/// entirely normal setup.
pub fn paths_related(a_norm: &str, b_norm: &str) -> bool {
    a_norm == b_norm
        || a_norm
            .strip_prefix(b_norm)
            .is_some_and(|rest| rest.starts_with('/'))
        || b_norm
            .strip_prefix(a_norm)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Every `workspaceStorage/<hash>/` directory whose folder is `target_norm`
/// (pre-normalized) **or related to it** ([`paths_related`]), as
/// `(hash-dir-name, dir-path)` pairs. Sorted by hash name for determinism.
pub fn related_workspace_storages(
    ws_storage_root: &Path,
    target_norm: &str,
) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(ws_storage_root) else {
        return out;
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        let Ok(text) = std::fs::read_to_string(dir.join("workspace.json")) else {
            continue;
        };
        let Some(folder) = parse_workspace_folder(&text) else {
            continue;
        };
        if paths_related(
            &normalize_for_compare(&folder.to_string_lossy()),
            target_norm,
        ) {
            let hash = entry.file_name().to_string_lossy().into_owned();
            out.push((hash, dir));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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
    fn paths_related_accepts_equal_ancestor_and_descendant_only() {
        let ws = normalize_for_compare("/work/challenge/stage-1-02");
        // The same folder, a parent project root, and a child folder all relate…
        assert!(paths_related(
            &normalize_for_compare("/work/challenge/stage-1-02"),
            &ws
        ));
        assert!(paths_related(
            &normalize_for_compare("/work/challenge"),
            &ws
        ));
        assert!(paths_related(
            &normalize_for_compare("/work/challenge/stage-1-02/sub"),
            &ws
        ));
        // …while a sibling level and a string-prefix non-boundary do not.
        assert!(!paths_related(
            &normalize_for_compare("/work/challenge/stage-1-01"),
            &ws
        ));
        assert!(!paths_related(
            &normalize_for_compare("/work/challenge/stage-1-02b"),
            &ws
        ));
    }

    #[test]
    fn related_storages_include_the_parent_project_root() {
        let base =
            std::env::temp_dir().join(format!("promptlyd-vscode-rel-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let root = base.join("workspaceStorage");
        for (hash, folder) in [
            ("aaaa", "file:///work/challenge"),          // parent — related
            ("bbbb", "file:///work/challenge/stage-02"), // exact — related
            ("cccc", "file:///work/other"),              // unrelated
        ] {
            let dir = root.join(hash);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("workspace.json"),
                format!(r#"{{"folder":"{folder}"}}"#),
            )
            .unwrap();
        }

        let target = normalize_for_compare("/work/challenge/stage-02");
        let found = related_workspace_storages(&root, &target);
        let hashes: Vec<&str> = found.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(hashes, vec!["aaaa", "bbbb"]);

        std::fs::remove_dir_all(&base).ok();
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
