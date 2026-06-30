//! `promptly help` — a grouped, styled overview of every command.
//!
//! clap's auto-generated `--help` is exhaustive but flat; this is the
//! at-a-glance map a player reaches for first, grouping commands by the part of
//! the workflow they belong to (solve → feedback → daemon → account → upkeep).
//! Rendering is pure (returns a `String`) so the layout is unit-tested without a
//! terminal; `run` just prints it. Colors stay restrained — accent for the
//! structure (brand + headings), bold for the actionable command token, dim for
//! the secondary detail — and every row aligns on the *visible* text so the ANSI
//! escapes (zero-width in the terminal) never throw the columns off.

use crate::style::Style;
use crate::CommandExit;

/// One row: the command (or flag) name, an optional argument hint, and a
/// one-line summary.
struct Entry {
    name: &'static str,
    args: &'static str,
    summary: &'static str,
}

impl Entry {
    /// The visible width of the name + argument hint (ASCII, so bytes == columns).
    fn width(&self) -> usize {
        self.name.len() + self.args.len()
    }
}

/// A titled group of related commands.
struct Group {
    title: &'static str,
    entries: &'static [Entry],
}

/// The command groups, in the order a player meets them: the core solve loop
/// first, then live feedback, the background daemon, the one-time account step,
/// and upkeep last.
const GROUPS: &[Group] = &[
    Group {
        title: "WORKFLOW",
        entries: &[
            Entry {
                name: "play",
                args: " [<level>]",
                summary: "Fetch a level, launch the daemon, and start capturing (fast path)",
            },
            Entry {
                name: "init",
                args: " <level>",
                summary: "Fetch a level's workspace and start the solve clock",
            },
            Entry {
                name: "start",
                args: "",
                summary: "Begin a scored capture session (launches the daemon)",
            },
            Entry {
                name: "stop",
                args: "",
                summary: "End the capture session",
            },
            Entry {
                name: "submit",
                args: "",
                summary: "Redact, package, and submit your solution for grading",
            },
        ],
    },
    Group {
        title: "FEEDBACK",
        entries: &[
            Entry {
                name: "watch",
                args: "",
                summary: "Stream live token burn and a running projected score",
            },
            Entry {
                name: "score",
                args: "",
                summary: "Show the projected score for the current attempt",
            },
            Entry {
                name: "test",
                args: "",
                summary: "Run the level's public tests locally",
            },
        ],
    },
    Group {
        title: "DAEMON",
        entries: &[
            Entry {
                name: "up",
                args: "",
                summary: "Start the background capture daemon (no session)",
            },
            Entry {
                name: "down",
                args: "",
                summary: "Stop the background capture daemon",
            },
        ],
    },
    Group {
        title: "ACCOUNT",
        entries: &[Entry {
            name: "pair",
            args: "",
            summary: "Link this device to your Promptly account",
        }],
    },
    Group {
        title: "MAINTENANCE",
        entries: &[
            Entry {
                name: "status",
                args: "",
                summary: "Show whether the daemon is running and capturing",
            },
            Entry {
                name: "doctor",
                args: "",
                summary: "Run a full setup diagnostic",
            },
            Entry {
                name: "reset",
                args: "",
                summary: "Restore the level's starter files (backs up first)",
            },
        ],
    },
];

/// The global flags every command shares (the clap `global = true` args).
const OPTIONS: &[Entry] = &[
    Entry {
        name: "--api-url",
        args: " <url>",
        summary: "Promptly web app URL (else PROMPTLY_API_URL)",
    },
    Entry {
        name: "--api-port",
        args: " <port>",
        summary: "Daemon control-API port (default 8765)",
    },
    Entry {
        name: "--no-color",
        args: "",
        summary: "Disable colored output",
    },
];

pub fn run(style: Style) -> anyhow::Result<CommandExit> {
    print!("{}", render(style));
    Ok(CommandExit::Success)
}

/// Render the whole help screen. Pure (returns the text) so it's unit-tested.
fn render(style: Style) -> String {
    let mut out = String::new();

    // Brand line: the name pops in accent + bold, the tagline recedes in dim.
    out.push('\n');
    out.push_str(&format!(
        "  {} {}\n",
        style.bold(&style.accent("promptly")),
        style.dim(&format!(
            "v{} · the competitive prompt-engineering arena",
            env!("CARGO_PKG_VERSION"),
        )),
    ));

    // Usage.
    out.push('\n');
    out.push_str(&format!("  {}\n", heading(style, "USAGE")));
    out.push_str(&format!(
        "    {} {}\n",
        style.bold("promptly"),
        style.dim("<command> [options]"),
    ));

    // Command groups share one summary column — set by the widest command across
    // all groups — so all the descriptions line up down the whole list. The
    // options block aligns within itself, since flags are naturally wider.
    let cmd_col = GROUPS
        .iter()
        .flat_map(|g| g.entries.iter())
        .map(Entry::width)
        .max()
        .unwrap_or(0);
    for group in GROUPS {
        out.push('\n');
        out.push_str(&section(style, group.title, group.entries, cmd_col));
    }
    out.push('\n');
    out.push_str(&section(
        style,
        "OPTIONS",
        OPTIONS,
        OPTIONS.iter().map(Entry::width).max().unwrap_or(0),
    ));

    // Footer: point at clap's per-command help for the full detail.
    out.push('\n');
    out.push_str(&format!(
        "  {}\n",
        style.dim("Run `promptly <command> --help` for the full detail on any command."),
    ));

    out
}

/// A titled section: the heading, then one aligned row per entry. `col` is the
/// width the name column is padded to before the summary — passed in so several
/// sections can share a column and line up together.
fn section(style: Style, title: &str, entries: &[Entry], col: usize) -> String {
    let mut out = format!("  {}\n", heading(style, title));
    for e in entries {
        // +2 is the gap between the name column and the summary.
        let pad = col.saturating_sub(e.width()) + 2;
        out.push_str(&format!(
            "    {}{}{}{}\n",
            style.bold(e.name),
            style.dim(e.args),
            " ".repeat(pad),
            e.summary,
        ));
    }
    out
}

/// A section heading: bold + accent so the uppercase labels anchor the eye.
fn heading(style: Style, title: &str) -> String {
    style.bold(&style.accent(title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_command_grouped_under_its_section() {
        let plain = render(Style::plain());

        for section in [
            "USAGE",
            "WORKFLOW",
            "FEEDBACK",
            "DAEMON",
            "ACCOUNT",
            "MAINTENANCE",
            "OPTIONS",
        ] {
            assert!(plain.contains(section), "missing section: {section}");
        }

        // Match each command as a row prefix (after the indent) so short names
        // like `up` don't false-match inside words such as "setup".
        let rows: Vec<String> = plain.lines().map(|l| l.trim_start().to_string()).collect();
        for cmd in [
            "play", "init", "start", "stop", "submit", "watch", "score", "test", "up", "down",
            "pair", "status", "doctor", "reset",
        ] {
            let found = rows
                .iter()
                .any(|l| l == cmd || l.starts_with(&format!("{cmd} ")));
            assert!(found, "missing command row: {cmd}");
        }

        // The brand and the per-command help hint frame the screen.
        assert!(plain.contains("promptly"));
        assert!(plain.contains("--help"));
    }

    #[test]
    fn does_not_resurface_the_removed_login_command() {
        assert!(!render(Style::plain()).contains("login"));
    }

    #[test]
    fn plain_render_emits_no_ansi_escapes() {
        // Plain style is what pipes/CI see; it must stay free of escape codes.
        assert!(!render(Style::plain()).contains('\x1b'));
    }

    #[test]
    fn section_aligns_summaries_regardless_of_name_width() {
        // A narrow and a wide entry: their summaries must start at the same
        // visible column, which is the whole point of the per-section padding.
        let entries = [
            Entry {
                name: "a",
                args: "",
                summary: "first",
            },
            Entry {
                name: "much-wider",
                args: " <arg>",
                summary: "second",
            },
        ];
        let col = entries.iter().map(Entry::width).max().unwrap();
        let text = section(Style::plain(), "TITLE", &entries, col);
        let rows: Vec<&str> = text.lines().skip(1).collect(); // skip the heading line
        let first = rows[0].find("first").unwrap();
        let second = rows[1].find("second").unwrap();
        assert_eq!(first, second);
    }
}
