//! The prompts, pinned: the seven playbooks are listed with their arguments,
//! a body is generated for the project's own languages, and a bad request is
//! refused with the reason.

use serde_json::json;

use super::*;

fn rust_and_axle() -> Context {
    Context {
        project: Some("axle".into()),
        langs: vec![
            LangShare {
                lang: Lang::Rust,
                chunks: 70,
            },
            LangShare {
                lang: Lang::Axle,
                chunks: 25,
            },
            LangShare {
                lang: Lang::Markdown,
                chunks: 5,
            },
        ],
    }
}

fn args(target: &str, scope: Option<&str>) -> Arguments {
    Arguments {
        target: Some(target.into()),
        scope: scope.map(str::to_string),
    }
}

#[test]
fn the_list_holds_the_six_workflows_and_bug_trace_each_with_target_and_scope() {
    let listed = list();
    let prompts = listed["prompts"].as_array().expect("array");
    let names: Vec<&str> = prompts.iter().filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(
        names,
        [
            "review",
            "refactor",
            "doc-sync",
            "divergence",
            "onboard",
            "security",
            "bug-trace"
        ]
    );
    for prompt in prompts {
        let arguments: Vec<&str> = prompt["arguments"]
            .as_array()
            .expect("arguments")
            .iter()
            .filter_map(|a| a["name"].as_str())
            .collect();
        assert_eq!(arguments, ["target", "scope"], "{prompt}");
        assert!(
            prompt["description"].as_str().is_some_and(|d| d.len() > 30),
            "{prompt}"
        );
    }
    assert_eq!(
        prompts[0]["arguments"][0]["required"], true,
        "review needs a target"
    );
    assert_eq!(
        prompts[2]["arguments"][0]["required"], false,
        "doc-sync does not"
    );
}

#[test]
fn a_review_of_a_rust_and_axle_project_carries_both_languages_constructs() {
    let review = playbooks::find("review").expect("review");
    let text = body(review, &args("Indexer::sync", None), &rust_and_axle());
    assert!(
        text.starts_with("# Review `Indexer::sync` in `axle`"),
        "{text}"
    );
    assert!(text.contains("intent: \"review\"}"), "{text}");
    assert!(
        text.contains("- Rust: look at") && text.contains("`impl Trait for` blocks"),
        "{text}"
    );
    assert!(text.contains("`#[cfg(test)]` modules"), "{text}");
    assert!(
        text.contains("- Axle: look at") && text.contains("`! E`"),
        "{text}"
    );
    assert!(text.contains("`own`"), "{text}");
    assert!(
        !text.contains("- Markdown"),
        "no review hint for Markdown: {text}"
    );
    assert!(
        !text.contains("- Go"),
        "only the project's languages: {text}"
    );
}

#[test]
fn a_scope_narrows_every_step_as_a_name_or_as_a_glob() {
    let review = playbooks::find("review").expect("review");
    let named = body(review, &args("x", Some("api")), &rust_and_axle());
    assert!(
        named.contains("intent: \"review\", scope: \"api\"}"),
        "{named}"
    );
    let glob = body(review, &args("x", Some("crates/core/**")), &rust_and_axle());
    assert!(glob.contains("paths: [\"crates/core/**\"]"), "{glob}");
}

#[test]
fn a_project_with_unknown_languages_still_gets_its_playbook() {
    let security = playbooks::find("security").expect("security");
    let unknown = Context {
        project: None,
        langs: Vec::new(),
    };
    let text = body(
        security,
        &Arguments {
            target: None,
            scope: None,
        },
        &unknown,
    );
    assert!(
        text.contains("# Security review `the whole project`"),
        "{text}"
    );
    assert!(
        text.contains("Unknown until the project is synced"),
        "{text}"
    );
}

#[test]
fn a_missing_target_an_unknown_argument_or_prompt_is_refused() {
    let review = playbooks::find("review").expect("review");
    let missing = Arguments::read(review, Some(&json!({ "scope": "api" })));
    assert!(
        missing
            .err()
            .is_some_and(|r| r.0.contains("needs `target`"))
    );
    let stray = Arguments::read(review, Some(&json!({ "target": "x", "depth": "3" })));
    assert!(
        stray
            .err()
            .is_some_and(|r| r.0.contains("no argument `depth`"))
    );
    assert!(playbooks::find("deploy").is_none());
}

#[test]
fn every_playbook_draws_on_intents_the_library_has() {
    let library = IntentLibrary::builtin().expect("the shipped library loads");
    for playbook in PLAYBOOKS {
        for name in playbook.hints_from.iter().chain(playbook.intent.iter()) {
            assert!(library.find(name).is_some(), "{}: `{name}`", playbook.name);
        }
    }
}
