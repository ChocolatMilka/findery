//! Shell completion: the script `findery completions` prints, the line that
//! loads it, and the answer to a TAB press.
//!
//! INTENT: two mechanisms, one command. The default script is a *registration*:
//! on every TAB the shell runs `findery` again with [`VAR`] set, and this
//! binary answers from the live surface — subcommands, flags, and the values
//! [`values`] knows: project ids, buckets, model aliases, intents, scopes. A
//! flag renamed is a completion renamed, with nothing to regenerate. `--static`
//! prints clap's self-contained script instead, for a shell that should never
//! run a program to complete a word; it knows the surface, not the values.
//!
//! The completer is the bare name `findery`, never this executable's path: the
//! shell resolves it the way it resolves the command being typed, so TAB always
//! asks the binary the line will run.
//!
//! `install` prints the line to add to a shell profile, and stops there. A
//! profile is the user's file; a tool that edits it edits someone else's file.

pub(crate) mod values;

use std::io::Write;

use clap::{Args, ValueEnum};
use clap_complete::CompleteEnv;
use clap_complete::env::{self, EnvCompleter};

use findery_core::{Error, Result};

/// The variable a registration script sets when it asks for candidates.
//
// Not clap's generic `COMPLETE`: a variable that common could be exported for
// some other tool, and would turn every `findery` command into a completer.
const VAR: &str = "FINDERY_COMPLETE";

/// The command a script registers, and the one it runs to complete it.
const BIN: &str = "findery";

/// `findery completions`.
#[derive(Args)]
pub(crate) struct CompletionsArgs {
    /// The shell to print a script for, or `install` for the line that loads it
    #[arg(value_name = "SHELL")]
    pub(crate) target: Target,
    /// A self-contained script: commands and flags, but no names under TAB
    #[arg(long = "static")]
    pub(crate) static_script: bool,
}

/// What `findery completions` prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Target {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
    /// The line that loads the script, for the shell this runs in
    Install,
}

/// A shell findery can complete in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
}

impl Target {
    fn shell(self) -> Option<Shell> {
        match self {
            Self::Bash => Some(Shell::Bash),
            Self::Zsh => Some(Shell::Zsh),
            Self::Fish => Some(Shell::Fish),
            Self::Powershell => Some(Shell::Powershell),
            Self::Elvish => Some(Shell::Elvish),
            Self::Install => None,
        }
    }
}

impl Shell {
    const ALL: [Shell; 5] = [
        Self::Bash,
        Self::Zsh,
        Self::Fish,
        Self::Powershell,
        Self::Elvish,
    ];

    /// The name `findery completions` takes, and the one `$SHELL` ends in.
    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Powershell => "powershell",
            Self::Elvish => "elvish",
        }
    }

    /// The registration clap's completion engine writes for this shell.
    fn registration(self) -> &'static dyn EnvCompleter {
        match self {
            Self::Bash => &env::Bash,
            Self::Zsh => &env::Zsh,
            Self::Fish => &env::Fish,
            Self::Powershell => &env::Powershell,
            Self::Elvish => &env::Elvish,
        }
    }

    /// The generator for the self-contained script.
    fn generator(self) -> clap_complete::Shell {
        match self {
            Self::Bash => clap_complete::Shell::Bash,
            Self::Zsh => clap_complete::Shell::Zsh,
            Self::Fish => clap_complete::Shell::Fish,
            Self::Powershell => clap_complete::Shell::PowerShell,
            Self::Elvish => clap_complete::Shell::Elvish,
        }
    }

    /// The profile a shell reads at startup.
    fn profile(self) -> &'static str {
        match self {
            Self::Bash => "~/.bashrc",
            Self::Zsh => "~/.zshrc",
            Self::Fish => "~/.config/fish/config.fish",
            Self::Powershell => "$PROFILE",
            Self::Elvish => "~/.config/elvish/rc.elv",
        }
    }

    /// The profile line that loads `script` — a command printing it — at
    /// every shell start.
    fn load_line(self, script: &str) -> String {
        match self {
            Self::Bash | Self::Zsh => format!("source <({script})"),
            Self::Fish => format!("{script} | source"),
            Self::Powershell => format!("{script} | Out-String | Invoke-Expression"),
            Self::Elvish => format!("eval ({script} | slurp)"),
        }
    }
}

/// Answers a TAB press and exits, when this process is one; returns otherwise.
pub(crate) fn intercept() {
    CompleteEnv::with_factory(crate::args::help::command)
        .var(VAR)
        .bin(BIN)
        .completer(BIN)
        .complete();
}

/// `findery completions`: the script, or the line that loads it, on stdout.
pub(crate) fn print(args: &CompletionsArgs) -> Result<()> {
    let text = match args.target.shell() {
        Some(shell) => script(shell, args.static_script)?,
        None => install(detect(), args.static_script),
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

/// The completion script for `shell`.
fn script(shell: Shell, static_script: bool) -> Result<String> {
    let mut buf = Vec::new();
    if static_script {
        clap_complete::generate(
            shell.generator(),
            &mut crate::args::help::command(),
            BIN,
            &mut buf,
        );
    } else {
        shell
            .registration()
            .write_registration(VAR, BIN, BIN, BIN, &mut buf)?;
    }
    String::from_utf8(buf)
        .map_err(|e| Error::Internal(format!("the completion script is not UTF-8: {e}")))
}

/// The two lines `install` prints: where the line goes, and the line. Both
/// are valid shell, so `findery completions install >> <profile>` works too.
fn install(shell: Shell, static_script: bool) -> String {
    let flag = if static_script { " --static" } else { "" };
    let script = format!("{BIN} completions {}{flag}", shell.name());
    format!(
        "# {}: add this line to {}, then open a new shell\n{}\n",
        shell.name(),
        shell.profile(),
        shell.load_line(&script)
    )
}

/// The shell this runs in, as well as the environment tells.
fn detect() -> Shell {
    detect_from(std::env::var("SHELL").ok().as_deref(), cfg!(windows))
}

/// `$SHELL` when it names a shell findery knows; otherwise PowerShell on
/// Windows, where `$SHELL` is unset outside Git Bash and WSL, and bash elsewhere.
fn detect_from(shell_var: Option<&str>, windows: bool) -> Shell {
    let named = shell_var
        .and_then(|path| path.rsplit(['/', '\\']).next())
        .map(|file| file.trim_end_matches(".exe"))
        .and_then(|name| match name {
            "pwsh" | "powershell" => Some(Shell::Powershell),
            other => Shell::ALL.into_iter().find(|s| s.name() == other),
        });
    match (named, windows) {
        (Some(shell), _) => shell,
        (None, true) => Shell::Powershell,
        (None, false) => Shell::Bash,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn every_shell_gets_a_registration_naming_the_binary_and_the_variable() {
        for shell in Shell::ALL {
            let text = script(shell, false).expect("a registration");
            assert!(
                text.contains(BIN) && text.contains(VAR),
                "{}:\n{text}",
                shell.name()
            );
        }
    }

    #[test]
    fn every_shell_gets_a_static_script_that_knows_the_surface_and_never_calls_back() {
        for shell in Shell::ALL {
            let text = script(shell, true).expect("a static script");
            let name = shell.name();
            assert!(text.contains(BIN), "{name} names the binary");
            assert!(text.contains("--intent"), "{name} knows the flags");
            assert!(!text.contains(VAR), "{name}: a static script runs nothing");
        }
    }

    #[test]
    fn every_target_but_install_is_a_shell_of_the_same_name() {
        for target in Target::value_variants() {
            let value = target.to_possible_value().expect("every target is visible");
            match target.shell() {
                Some(shell) => assert_eq!(shell.name(), value.get_name()),
                None => assert_eq!(*target, Target::Install),
            }
        }
    }

    #[test]
    fn install_prints_where_the_line_goes_and_a_line_that_loads_the_script() {
        for shell in Shell::ALL {
            let text = install(shell, false);
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 2, "{text}");
            assert!(lines[0].starts_with('#') && lines[0].contains(shell.profile()));
            assert!(lines[1].contains(&format!("findery completions {}", shell.name())));
        }
        assert_eq!(
            install(Shell::Powershell, false).lines().nth(1),
            Some("findery completions powershell | Out-String | Invoke-Expression")
        );
        assert!(install(Shell::Bash, true).contains("source <(findery completions bash --static)"));
    }

    #[test]
    fn the_shell_is_read_from_shell_then_from_the_platform() {
        assert_eq!(detect_from(Some("/usr/bin/zsh"), false), Shell::Zsh);
        assert_eq!(
            detect_from(Some("/opt/homebrew/bin/fish"), false),
            Shell::Fish
        );
        assert_eq!(
            detect_from(Some("C:\\Program Files\\Git\\usr\\bin\\bash.exe"), true),
            Shell::Bash
        );
        assert_eq!(detect_from(Some("/usr/bin/pwsh"), false), Shell::Powershell);
        assert_eq!(detect_from(None, true), Shell::Powershell);
        assert_eq!(detect_from(None, false), Shell::Bash);
        assert_eq!(detect_from(Some("/bin/tcsh"), true), Shell::Powershell);
    }

    /// What a TAB press gets, through clap's engine and the real surface.
    fn tab(words: &[&str]) -> Vec<String> {
        let mut cmd = crate::args::help::command();
        cmd.build();
        let args: Vec<OsString> = words.iter().map(OsString::from).collect();
        let index = args.len() - 1;
        clap_complete::engine::complete(&mut cmd, args, index, None)
            .expect("the engine answers")
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_tab_press_offers_commands_vocabularies_and_intents() {
        assert!(tab(&["findery", "se"]).contains(&"search".to_string()));
        assert_eq!(tab(&["findery", "search", "--lang", "ru"]), ["rust"]);
        assert!(tab(&["findery", "completions", ""]).contains(&"powershell".to_string()));
        assert!(
            tab(&["findery", "search", "--intent", "doc-"]).contains(&"doc-sync".to_string()),
            "intents complete from the built-in library"
        );
        assert!(
            !tab(&["findery", ""]).contains(&"daemon".to_string()),
            "a hidden command stays hidden"
        );
    }
}
