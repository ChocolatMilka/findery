//! What a person reads in `--help`, pinned: every command grouped once, every
//! command illustrated, every example runnable, no rationale leaking.

use clap::Parser;

use super::examples::EXAMPLES;
use super::*;

/// The surface as `--help` shows it: built, so global options and bin names
/// are propagated to every subcommand.
pub(crate) fn built() -> Command {
    let mut command = command();
    command.build();
    command
}

/// The visible subcommands of `command`, clap's own `help` aside.
fn visible(command: &Command) -> impl Iterator<Item = &Command> {
    command
        .get_subcommands()
        .filter(|c| !c.is_hide_set() && c.get_name() != "help")
}

/// Every help page under `command` and itself, short and long, labelled with
/// the command path — hidden commands too when `hidden` is set.
pub(crate) fn pages(command: &Command, hidden: bool) -> Vec<(String, String)> {
    let label = command
        .get_bin_name()
        .unwrap_or(command.get_name())
        .to_string();
    let mut out = vec![
        (label.clone(), command.clone().render_help().to_string()),
        (label, command.clone().render_long_help().to_string()),
    ];
    for sub in command.get_subcommands() {
        if hidden || !sub.is_hide_set() {
            out.extend(pages(sub, hidden));
        }
    }
    out
}

#[test]
fn every_visible_command_is_listed_in_exactly_one_group() {
    let root = built();
    let listed: Vec<&str> = COMMAND_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let shown: Vec<&str> = visible(&root).map(Command::get_name).collect();
    for name in &shown {
        let times = listed.iter().filter(|l| *l == name).count();
        assert_eq!(
            times, 1,
            "`{name}` is listed {times} times in COMMAND_GROUPS"
        );
    }
    for name in &listed {
        assert!(
            shown.contains(name),
            "COMMAND_GROUPS lists `{name}`, which is not a visible command"
        );
    }
}

#[test]
fn the_root_page_lists_each_group_under_its_heading_with_each_summary() {
    let root = built();
    let text = root.clone().render_long_help().to_string();
    let mut from = 0;
    for (title, names) in COMMAND_GROUPS {
        let heading = format!("\n{title}:\n");
        let at = from
            + text[from..]
                .find(&heading)
                .unwrap_or_else(|| panic!("no `{title}` heading, in order, in\n{text}"));
        let lines = text[at + heading.len()..].lines().take(names.len());
        for (line, name) in lines.zip(names.iter()) {
            let about = root
                .find_subcommand(name)
                .and_then(Command::get_about)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert_eq!(line.split_whitespace().next(), Some(*name), "{text}");
            assert!(
                line.ends_with(&about),
                "`{name}` shows its own summary: {line}"
            );
        }
        from = at + heading.len();
    }
    for part in [
        "Usage: findery [OPTIONS] <COMMAND>",
        "\nOptions:\n",
        "--no-spawn",
    ] {
        assert!(text.contains(part), "`{part}` missing from\n{text}");
    }
    assert!(text.contains("'findery <command> --help'"), "{text}");
}

#[test]
fn every_command_ends_its_long_help_with_examples_and_keeps_its_short_help_short() {
    let root = built();
    for sub in visible(&root) {
        let name = sub.get_name();
        let long = sub.clone().render_long_help().to_string();
        let short = sub.clone().render_help().to_string();
        assert!(long.contains("\nExamples:\n"), "`{name} --help`:\n{long}");
        assert!(
            long.contains(&format!("  findery {name}")),
            "`{name}`:\n{long}"
        );
        assert!(
            !short.contains("Examples:"),
            "`{name} -h` must stay a summary"
        );
        assert!(short.contains("Global options:"), "`{name} -h`:\n{short}");
    }
    let named: Vec<&str> = visible(&root).map(Command::get_name).collect();
    for (command, _) in EXAMPLES {
        assert!(
            named.contains(command),
            "examples for `{command}`, not a visible command"
        );
    }
}

/// The `findery …` part of an example line, split as a shell would: from the
/// first `findery` to the first pipe or redirection, double quotes kept whole.
fn invocation(line: &str) -> Vec<String> {
    let start = line.find("findery ").expect("an example runs findery");
    let (mut words, mut word, mut quoted) = (Vec::new(), String::new(), false);
    for ch in line[start..].chars() {
        match ch {
            '"' => quoted = !quoted,
            '|' | '>' | '<' | ')' if !quoted => break,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// An example is a promise: the line a reader copies must be one the surface
/// accepts, so a renamed flag fails here before it fails someone's paste.
#[test]
fn every_example_is_a_command_line_the_surface_accepts() {
    for (command, examples) in EXAMPLES {
        for example in examples.iter() {
            let words = invocation(example.line);
            if let Err(e) = Cli::try_parse_from(&words) {
                panic!("`{command}` example `{}` is refused:\n{e}", example.line);
            }
            assert!(
                !example.what.is_empty(),
                "`{}` says what it does",
                example.line
            );
        }
    }
}

#[test]
fn no_rendered_help_leaks_an_internal_rationale() {
    for (name, page) in pages(&built(), true) {
        assert!(
            !page.contains("INTENT:"),
            "`{name}` help leaks a rationale:\n{page}"
        );
    }
}

/// A brace in a summary would be read as a template tag by the root page,
/// and clap drops a tag it does not know along with the text after it.
#[test]
fn no_summary_can_break_the_root_template() {
    for sub in built().get_subcommands() {
        let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
        assert!(!about.contains(['{', '}']), "`{}`: {about}", sub.get_name());
    }
}

#[test]
fn the_palette_reaches_the_help_and_plain_output_carries_no_escape() {
    let page = built().render_long_help();
    let header = style::HELP.get_header().render().to_string();
    assert!(
        page.ansi()
            .to_string()
            .contains(&format!("{header}Search:")),
        "group headings use the header style"
    );
    assert!(!page.to_string().contains('\x1b'), "plain text is plain");
}
