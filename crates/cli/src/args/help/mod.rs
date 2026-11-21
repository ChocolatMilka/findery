//! The help a person reads: commands grouped by what they are for, one
//! palette, and an examples block at the end of every long help.
//!
//! INTENT: clap lists subcommands flat, in declaration order — `export` beside
//! `watch`, `doctor` after `health` — which is the order the code grew in, not
//! the order anyone looks for a command in. The grouping is one table,
//! [`COMMAND_GROUPS`], and a test fails when a visible command is missing from
//! it or listed twice, so a command added later cannot vanish from the help.
//! The table holds names only: each line reads the subcommand's own `about`,
//! so a summary is written once, next to the command it describes.
//!
//! The root page is a clap template with the groups written into it. Every
//! escape code in it is written unconditionally ([`styled`]); clap strips them
//! when the output is not a terminal or `NO_COLOR` is set, exactly as it does
//! for its own.

mod examples;

use std::fmt::Write as _;

use clap::{Command, CommandFactory, FromArgMatches};

use crate::args::Cli;
use crate::ui::style::{self, styled};

/// Every visible command, under the heading it is listed with, in reading
/// order: first steps, then the daily loop, then the things set once.
pub(crate) const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Get started", &["init", "doctor", "health"]),
    ("Search", &["search", "intents"]),
    ("Index", &["sync", "watch", "status", "jobs"]),
    ("Projects", &["projects", "detach", "rm", "doc"]),
    ("Models & storage", &["model", "bucket", "export"]),
    ("Settings", &["config", "completions"]),
];

/// The argument surface with its help dressed: the palette, the grouped root
/// page and the examples. Parsing, completion and the tests all start here,
/// so what they see is what a person reads.
pub(crate) fn command() -> Command {
    let command = Cli::command().styles(style::HELP);
    let template = root_template(&command);
    examples::attach(command.help_template(template).after_help(footer()))
}

/// The process arguments parsed against [`command`]; a usage error exits with
/// clap's message, as `Parser::parse` would.
pub(crate) fn parse() -> Cli {
    let mut command = command();
    let matches = command.get_matches_mut();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.format(&mut command).exit())
}

/// `Heading:` in the header style.
fn heading(title: &str) -> String {
    styled(*style::HELP.get_header(), &format!("{title}:"))
}

/// Text to type as written, in the literal style.
fn literal(text: &str) -> String {
    styled(*style::HELP.get_literal(), text)
}

/// The root page: summary, usage, the command groups, the options, the footer.
fn root_template(command: &Command) -> String {
    let width = COMMAND_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    let groups: String = COMMAND_GROUPS
        .iter()
        .map(|(title, names)| group(command, title, names, width))
        .collect();
    format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n{groups}\n{}\n{{options}}{{after-help}}",
        heading("Options")
    )
}

/// One group: its heading, then a line per command, names padded to `width`.
fn group(command: &Command, title: &str, names: &[&str], width: usize) -> String {
    let mut out = format!("\n{}\n", heading(title));
    for name in names {
        let summary = command
            .find_subcommand(name)
            .and_then(Command::get_about)
            .map(ToString::to_string)
            .unwrap_or_default();
        let _ = writeln!(out, "  {}  {summary}", literal(&format!("{name:<width$}")));
    }
    out
}

/// What the root page ends with: the two habits worth knowing first.
fn footer() -> String {
    format!(
        "Most commands need no {}: the project containing the current directory is used.\n\
         See '{}' for details and examples.",
        literal("-p"),
        literal("findery <command> --help"),
    )
}

#[cfg(test)]
pub(crate) mod tests;
