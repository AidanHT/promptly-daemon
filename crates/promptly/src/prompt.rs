//! Interactive yes/no prompts, behind a trait so command flows are testable.
//!
//! A real [`StdinAsk`] reads the terminal; tests inject a scripted answerer. Each
//! prompt declares two distinct fallbacks — `on_empty` (the user pressed Enter)
//! and `on_noninteractive` (stdin isn't a TTY) — so a non-interactive `start`
//! never *silently* writes settings: consent defaults to "no" off a TTY, while a
//! reset defaults to "abort". `--yes` is applied by the commands (which wrap the
//! asker), not here, so it stays testable with a scripted answerer.

use std::io::{self, IsTerminal, Write};

/// A source of yes/no answers.
pub trait Ask {
    /// Ask `question`; `on_empty` is used when the user just presses Enter and
    /// `on_noninteractive` when there is no TTY to ask.
    fn confirm(&mut self, question: &str, on_empty: bool, on_noninteractive: bool) -> bool;
}

/// Reads answers from the real terminal.
#[derive(Debug, Default)]
pub struct StdinAsk;

impl StdinAsk {
    pub fn new() -> Self {
        Self
    }
}

impl Ask for StdinAsk {
    fn confirm(&mut self, question: &str, on_empty: bool, on_noninteractive: bool) -> bool {
        if !io::stdin().is_terminal() {
            return on_noninteractive;
        }
        let hint = if on_empty { "Y/n" } else { "y/N" };
        loop {
            print!("{question} [{hint}] ");
            let _ = io::stdout().flush();
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_err() {
                return on_noninteractive;
            }
            match line.trim().to_lowercase().as_str() {
                "" => return on_empty,
                "y" | "yes" => return true,
                "n" | "no" => return false,
                _ => continue, // re-ask on anything else
            }
        }
    }
}

#[cfg(test)]
pub use test_support::ScriptedAsk;

#[cfg(test)]
mod test_support {
    use super::Ask;
    use std::collections::VecDeque;

    /// A scripted answerer for tests: returns queued answers in order, then falls
    /// back to `on_empty` once the script is exhausted.
    pub struct ScriptedAsk {
        answers: VecDeque<bool>,
    }

    impl ScriptedAsk {
        pub fn new(answers: impl IntoIterator<Item = bool>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
            }
        }
    }

    impl Ask for ScriptedAsk {
        fn confirm(&mut self, _question: &str, on_empty: bool, _on_noninteractive: bool) -> bool {
            self.answers.pop_front().unwrap_or(on_empty)
        }
    }
}
