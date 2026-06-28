//! Terminal styling — colorized, cyberpunk-leaning output within terminal
//! limits (`19` UX). Honors `NO_COLOR` and only emits escapes to a real TTY, so
//! piped/redirected output and CI logs stay clean.
//!
//! The whole surface routes through a [`Style`] value resolved once at startup,
//! so a single `--no-color`/`NO_COLOR`/not-a-tty decision applies everywhere and
//! tests can force plain output deterministically.

use std::io::IsTerminal;

/// ANSI SGR codes used by the CLI (a deliberately small palette).
mod code {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const CYAN: &str = "\x1b[36m";
}

/// Whether colorized output is enabled. Clone-cheap; thread it through commands.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    enabled: bool,
}

impl Style {
    /// Resolve styling from the environment: disabled when `NO_COLOR` is present
    /// (any value, per the convention), when `force_plain` is set (`--no-color`),
    /// or when the stream isn't a TTY.
    pub fn resolve(force_plain: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            enabled: !force_plain && !no_color && std::io::stdout().is_terminal(),
        }
    }

    /// Plain (no escapes) — for tests and non-TTY output.
    pub fn plain() -> Self {
        Self { enabled: false }
    }

    pub fn is_enabled(self) -> bool {
        self.enabled
    }

    fn wrap(self, codes: &str, text: &str) -> String {
        if self.enabled {
            format!("{codes}{text}{}", code::RESET)
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.wrap(code::BOLD, text)
    }
    pub fn dim(self, text: &str) -> String {
        self.wrap(code::DIM, text)
    }
    pub fn red(self, text: &str) -> String {
        self.wrap(code::RED, text)
    }
    pub fn green(self, text: &str) -> String {
        self.wrap(code::GREEN, text)
    }
    pub fn yellow(self, text: &str) -> String {
        self.wrap(code::YELLOW, text)
    }
    pub fn magenta(self, text: &str) -> String {
        self.wrap(code::MAGENTA, text)
    }
    /// The accent color — used for the brand prefix and headings.
    pub fn accent(self, text: &str) -> String {
        self.wrap(code::CYAN, text)
    }

    /// A green check or red cross for pass/fail lines (`doctor`, `test`).
    pub fn mark(self, ok: bool) -> String {
        if ok {
            self.green("✓")
        } else {
            self.red("✗")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_style_emits_no_escapes() {
        let s = Style::plain();
        assert_eq!(s.green("ok"), "ok");
        assert_eq!(s.bold("hi"), "hi");
        assert_eq!(s.mark(true), "✓");
        assert_eq!(s.mark(false), "✗");
        assert!(!s.is_enabled());
    }

    #[test]
    fn enabled_style_wraps_with_reset() {
        // Construct an enabled style directly (resolve() depends on the ambient
        // TTY, which tests must not rely on).
        let s = Style { enabled: true };
        let green = s.green("x");
        assert!(green.starts_with("\x1b[32m"));
        assert!(green.ends_with("\x1b[0m"));
        assert!(green.contains('x'));
    }
}
