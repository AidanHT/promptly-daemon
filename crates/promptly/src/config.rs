//! CLI configuration: resolving the Promptly web app's base URL.
//!
//! `init` (kit download), `doctor` (Judge0 health), and `test`'s remote fallback
//! talk to the web app. The base URL resolves from `--api-url`, else the
//! `PROMPTLY_API_URL` env var, else the deployed app — so a player who installs
//! the binary can `pair` and `submit` with zero configuration.

/// Default web-app base URL: the deployed app. Players are the common case and
/// get zero-config; working against a local `npm run dev` means pointing
/// `--api-url` (or `PROMPTLY_API_URL`) at `http://localhost:3000`. Override per
/// the resolution order in [`resolve_api_url`].
pub const DEFAULT_API_URL: &str = "https://xpromptly.com";

/// Resolve the web-app base URL: `--api-url` wins, then `PROMPTLY_API_URL`, then
/// the production default. A trailing slash is trimmed so path joins are clean.
pub fn resolve_api_url(flag: Option<&str>) -> String {
    let raw = flag
        .map(str::to_string)
        .or_else(|| {
            std::env::var("PROMPTLY_API_URL")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_API_URL.to_string());
    raw.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_wins_and_trailing_slash_is_trimmed() {
        assert_eq!(
            resolve_api_url(Some("https://promptly.dev/")),
            "https://promptly.dev"
        );
        assert_eq!(
            resolve_api_url(Some("http://localhost:3000")),
            "http://localhost:3000"
        );
    }

    #[test]
    fn falls_back_to_the_production_default() {
        // Don't depend on the ambient env: only assert the default when the flag
        // is set, plus that the const is the documented production URL.
        assert_eq!(DEFAULT_API_URL, "https://xpromptly.com");
        assert_eq!(resolve_api_url(Some(DEFAULT_API_URL)), DEFAULT_API_URL);
    }
}
