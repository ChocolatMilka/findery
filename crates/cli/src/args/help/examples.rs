//! The examples block every long help ends with.
//!
//! INTENT: a table, not prose. One [`Example`] per line a person would type,
//! with what it does, so adding one is one line — and a test parses every
//! command line against the real surface, so an example that stopped working
//! fails the build instead of the reader.

use clap::Command;

use super::{heading, literal};
use crate::ui::style::{DIM, styled};

/// One line to type, and what it does.
pub(super) struct Example {
    pub(super) what: &'static str,
    pub(super) line: &'static str,
}

const fn ex(what: &'static str, line: &'static str) -> Example {
    Example { what, line }
}

const INIT: &[Example] = &[
    ex(
        "Register the current directory; index it later",
        "findery init",
    ),
    ex("Register and index in one go", "findery init --sync"),
    ex(
        "Pin the project to a model profile",
        "findery init --model qwen3",
    ),
    ex(
        "Share a bucket, and index only Rust and Markdown",
        "findery init --bucket work --ext rs,md",
    ),
];

const DOCTOR: &[Example] = &[
    ex(
        "What is missing, and the command that fixes it",
        "findery doctor",
    ),
    ex("The same report, for a script", "findery doctor --json"),
];

const HEALTH: &[Example] = &[
    ex(
        "Whether the index answers, and what it serves",
        "findery health",
    ),
    ex(
        "Ask a server on another machine",
        "findery health --server http://10.0.0.5:8077",
    ),
];

const SEARCH: &[Example] = &[
    ex(
        "Search the current project by meaning and by name",
        "findery search \"where backoff is handled\"",
    ),
    ex(
        "Who calls a symbol",
        "findery search --intent usage \"Indexer::sync\"",
    ),
    ex(
        "The docs that describe a behaviour, to check them against the code",
        "findery search --intent doc-sync \"how buckets are removed\"",
    ),
    ex(
        "Chunks that look like this one; nothing is embedded",
        "findery search --like src/lib.rs:L40-L80",
    ),
    ex(
        "Narrow to a declared scope and one language, before scoring",
        "findery search --scope api --lang rust \"retry policy\"",
    ),
    ex(
        "Another project, five hits",
        "findery search -p axle -k 5 \"config loading\"",
    ),
];

const INTENTS: &[Example] = &[
    ex(
        "The intents this project's searches can name",
        "findery intents",
    ),
    ex(
        "The same list for another project",
        "findery intents -p axle",
    ),
];

const SYNC: &[Example] = &[
    ex("Index what changed since the last sync", "findery sync"),
    ex(
        "Re-read every file; vectors already computed are reused",
        "findery sync --full",
    ),
    ex(
        "Only two paths",
        "findery sync --path src/lib.rs --path README.md",
    ),
    ex(
        "Return at once, then follow it with `findery jobs`",
        "findery sync --background",
    ),
];

const WATCH: &[Example] = &[
    ex("Re-sync on every change", "findery watch"),
    ex(
        "React faster to a burst of saves",
        "findery watch --debounce-ms 500",
    ),
    ex("Stop watching", "findery watch --off"),
];

const STATUS: &[Example] = &[
    ex(
        "Files, chunks and vectors of this project",
        "findery status",
    ),
    ex("Of another project", "findery status -p axle"),
];

const JOBS: &[Example] = &[
    ex("Every background job", "findery jobs"),
    ex(
        "One job, as `sync --background` named it",
        "findery jobs 6f1c2a9e-3b7d-4c55-9a0e-2d8f41b7c310",
    ),
];

const PROJECTS: &[Example] = &[
    ex("Every project, the current one marked", "findery projects"),
    ex("The same list, for a script", "findery projects --json"),
];

const DETACH: &[Example] = &[
    ex(
        "Release this project's index files and keep them on disk",
        "findery detach",
    ),
    ex(
        "Attach it again later; nothing is re-indexed",
        "findery init",
    ),
    ex("Delete the index instead", "findery rm"),
];

const RM: &[Example] = &[
    ex("Delete this project's index", "findery rm"),
    ex("Delete another project's", "findery rm -p old-prototype"),
    ex("Keep the index, only release its files", "findery detach"),
];

const DOC: &[Example] = &[
    ex("Index one file now", "findery doc add docs/architecture.md"),
    ex(
        "Index text with no file behind it",
        "echo \"We retry three times.\" | findery doc add notes/retry.md --stdin",
    ),
    ex("Drop it again", "findery doc rm notes/retry.md"),
];

const MODEL: &[Example] = &[
    ex("Every model, the default marked", "findery model ls"),
    ex("One model in detail", "findery model show harrier"),
    ex("Make it the default", "findery model use harrier"),
    ex(
        "Switch back; the old vectors are still there",
        "findery model use qwen3",
    ),
];

const BUCKET: &[Example] = &[
    ex("Every bucket, one line per model file", "findery bucket ls"),
    ex(
        "A bucket several projects can share",
        "findery bucket create work",
    ),
    ex(
        "Delete one model's file, keep the others",
        "findery bucket rm axle --model harrier",
    ),
    ex(
        "Delete a bucket that still holds projects",
        "findery bucket rm work --force",
    ),
];

const EXPORT: &[Example] = &[
    ex("Vectors and metadata as JSON lines", "findery export"),
    ex(
        "Raw vectors into a directory",
        "findery export --format binary --dir ./vectors",
    ),
];

const CONFIG: &[Example] = &[
    ex(
        "Use an embedding runtime already running",
        "findery config set url http://127.0.0.1:11434",
    ),
    ex(
        "Point every command at a server elsewhere",
        "findery config set server http://10.0.0.5:8077",
    ),
    ex("What is set, secrets masked", "findery config show"),
    ex("Back to the default", "findery config unset url"),
];

const COMPLETIONS: &[Example] = &[
    ex(
        "The line to add to your shell's profile",
        "findery completions install",
    ),
    ex(
        "PowerShell, for this session",
        "findery completions powershell | Out-String | Invoke-Expression",
    ),
    ex(
        "Bash, for this session",
        "source <(findery completions bash)",
    ),
    ex(
        "A script that never runs findery on TAB",
        "findery completions fish --static > ~/.config/fish/completions/findery.fish",
    ),
];

/// Every command's examples, by command name.
pub(super) const EXAMPLES: &[(&str, &[Example])] = &[
    ("init", INIT),
    ("doctor", DOCTOR),
    ("health", HEALTH),
    ("search", SEARCH),
    ("intents", INTENTS),
    ("sync", SYNC),
    ("watch", WATCH),
    ("status", STATUS),
    ("jobs", JOBS),
    ("projects", PROJECTS),
    ("detach", DETACH),
    ("rm", RM),
    ("doc", DOC),
    ("model", MODEL),
    ("bucket", BUCKET),
    ("export", EXPORT),
    ("config", CONFIG),
    ("completions", COMPLETIONS),
];

/// Ends each command's long help with its examples; `-h` stays one screen.
pub(super) fn attach(command: Command) -> Command {
    command.mut_subcommands(|sub| match block(sub.get_name()) {
        Some(text) => sub.after_long_help(text),
        None => sub,
    })
}

/// `Examples:`, then each example as a comment and the line to type.
fn block(name: &str) -> Option<String> {
    let (_, examples) = EXAMPLES.iter().find(|(command, _)| *command == name)?;
    let entries: Vec<String> = examples
        .iter()
        .map(|e| {
            let what = styled(DIM, &format!("# {}", e.what));
            format!("  {what}\n  {}", literal(e.line))
        })
        .collect();
    Some(format!("{}\n{}", heading("Examples"), entries.join("\n\n")))
}
