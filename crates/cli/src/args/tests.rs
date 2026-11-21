//! The argument surface parses what the documentation says it parses.

use findery_core::support::path_map;

use super::*;

#[test]
fn the_deepest_matching_root_wins() {
    // `current_project` reads the real cwd, so the selection predicate is
    // exercised directly with a fixed one.
    let roots = [
        ("outer", "C:/dev"),
        ("inner", "C:/dev/app"),
        ("other", "D:/work"),
    ];
    let cwd = "C:/dev/app/crates/core";
    let mut best: Option<(usize, &str)> = None;
    for (id, root) in roots {
        if path_map::is_within(root, cwd) {
            let depth = path_map::normalize(root).len();
            if best.as_ref().is_none_or(|(b, _)| depth > *b) {
                best = Some((depth, id));
            }
        }
    }
    assert_eq!(best.map(|(_, id)| id), Some("inner"));
}

#[test]
fn a_sibling_directory_is_not_a_match() {
    assert!(!path_map::is_within("C:/dev/app", "C:/dev/app-old/src"));
    assert!(path_map::is_within("C:/dev/app", "c:/dev/app/src"));
    assert!(path_map::is_within("C:/dev/app", "C:/dev/app"));
}

#[test]
fn a_project_on_another_disk_is_reachable() {
    assert!(path_map::is_within(
        "D:/work/tool",
        "D:/work/tool/src/lib.rs"
    ));
    assert!(!path_map::is_within("C:/dev", "D:/work/tool"));
}

#[test]
fn the_cli_parses_a_cwd_scoped_search() {
    let cli = Cli::try_parse_from(["findery", "search", "where", "is", "retry"]).expect("parse");
    match cli.command {
        Command::Search { query, scope, .. } => {
            assert_eq!(query.join(" "), "where is retry");
            assert!(
                scope.project.is_none(),
                "no -p means: use the current directory"
            );
        }
        _ => panic!("expected a search"),
    }
}

#[test]
fn an_explicit_project_still_works_from_anywhere() {
    let cli = Cli::try_parse_from(["findery", "search", "-p", "axle", "retry"]).expect("parse");
    match cli.command {
        Command::Search { scope, .. } => assert_eq!(scope.project.as_deref(), Some("axle")),
        _ => panic!("expected a search"),
    }
}

#[test]
fn init_registers_without_indexing_unless_asked() {
    let cli = Cli::try_parse_from(["findery", "init"]).expect("parse");
    match cli.command {
        Command::Init {
            path,
            id,
            bucket,
            sync,
            ..
        } => {
            assert_eq!(path, ".");
            assert!(id.is_none());
            assert!(bucket.is_none(), "the configured default applies");
            assert!(!sync, "indexing is a separate, deliberate step");
        }
        _ => panic!("expected init"),
    }
}

#[test]
fn init_can_choose_a_bucket_and_index_in_one_go() {
    let cli =
        Cli::try_parse_from(["findery", "init", "--bucket", "work", "--sync"]).expect("parse");
    match cli.command {
        Command::Init { bucket, sync, .. } => {
            assert_eq!(bucket.as_deref(), Some("work"));
            assert!(sync);
        }
        _ => panic!("expected init"),
    }
}

#[test]
fn the_bucket_command_names_what_it_acts_on() {
    let cli = Cli::try_parse_from(["findery", "bucket", "create", "work"]).expect("parse");
    match cli.command {
        Command::Bucket {
            action: BucketAction::Create { name },
        } => assert_eq!(name, "work"),
        _ => panic!("expected bucket create"),
    }
    let cli = Cli::try_parse_from(["findery", "bucket", "rm", "work"]).expect("parse");
    match cli.command {
        Command::Bucket {
            action: BucketAction::Rm { name, force },
        } => {
            assert_eq!(name, "work");
            assert!(!force);
        }
        _ => panic!("expected bucket rm"),
    }
}

#[test]
fn no_daemon_address_is_baked_into_the_argument_surface() {
    // The file has to be able to speak below the flag and the environment,
    // so clap must not fill the field with a default of its own.
    let cli = Cli::try_parse_from(["findery", "projects"]).expect("parse");
    assert!(cli.server.is_none());
    assert!(cli.token.is_none());
}

#[test]
fn the_command_surface_stays_parseable() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
}

/// The `--ext` help spells the default list out, so it must be the list:
/// a language added to the chunkers and not to the help is one a user
/// never learns is indexed.
#[test]
fn the_init_help_lists_every_default_extension() {
    use clap::CommandFactory;
    let surface = Cli::command();
    let init = surface
        .get_subcommands()
        .find(|c| c.get_name() == "init")
        .expect("`init` exists");
    let ext = init
        .get_arguments()
        .find(|a| a.get_id() == "ext")
        .expect("`init --ext` exists");
    let help = ext
        .get_long_help()
        .or(ext.get_help())
        .expect("documented")
        .to_string();
    let listed = help.split('`').nth(1).expect("the list is quoted");
    assert_eq!(
        listed.split(',').collect::<Vec<_>>(),
        findery_core::config::DEFAULT_EXTENSIONS,
        "{help}"
    );
}

/// The spelling every document uses has to be the spelling that works.
/// `FINDERY_NO_SPAWN=1` is written in the compose file and in three pages;
/// clap's default parser accepts only `true` and `false`, which made a
/// documented setting fail at startup.
#[test]
fn the_no_spawn_variable_accepts_the_spelling_we_document() {
    // SAFETY: the variable is removed before the assertion, and no other
    // test in this binary reads it.
    unsafe { std::env::set_var("FINDERY_NO_SPAWN", "1") };
    let parsed = Cli::try_parse_from(["findery", "status"]);
    unsafe { std::env::remove_var("FINDERY_NO_SPAWN") };

    assert!(
        parsed
            .expect("`FINDERY_NO_SPAWN=1` is the documented spelling")
            .no_spawn,
        "the documented value must set the flag"
    );
}

/// The server is plumbing: it starts itself, so offering to manage it is
/// offering to manage something the user never asked for.
#[test]
fn the_server_lifecycle_is_reachable_but_not_advertised() {
    use clap::CommandFactory;
    let surface = Cli::command();
    let daemon = surface
        .get_subcommands()
        .find(|c| c.get_name() == "daemon")
        .expect("the escape hatch still exists for a service manager");
    assert!(daemon.is_hide_set(), "`daemon` must stay out of --help");
}

/// Nothing visible may mention the server by its process name: a command
/// that narrates its own plumbing teaches the concept it was meant to hide.
///
/// Walks the whole surface rather than a list, because the failure mode is a
/// command added later with a careless `///` line — and a list would let it
/// through.
#[test]
fn nothing_visible_ever_names_the_daemon() {
    use clap::CommandFactory;
    let surface = Cli::command();
    for command in surface.get_subcommands() {
        if command.is_hide_set() {
            continue;
        }
        // `render_long_help` covers the summary, the flag descriptions and
        // the subcommands in one page — the whole surface a user can read.
        let name = command.get_name().to_string();
        let text = command.clone().render_long_help().to_string();
        assert!(
            !text.to_lowercase().contains("daemon"),
            "`{name}` mentions the daemon:\n{text}"
        );
    }
}
