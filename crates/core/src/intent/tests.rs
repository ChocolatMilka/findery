//! The shipped library and its layers: the data file is only as good as what
//! the resolution does with it.

use super::*;
use crate::embedding::prompt::DEFAULT_QUERY_INSTRUCTION;
use crate::ids::ProjectId;
use crate::model::{SearchFilter, SearchRequest};

fn builtin() -> &'static IntentLibrary {
    IntentLibrary::builtin().expect("the shipped library loads")
}

fn overrides(src: &str) -> BTreeMap<String, IntentOverride> {
    toml::from_str(src).expect("an override table parses")
}

const LOCATE: &[&str] = &[
    "code",
    "symbol",
    "usage",
    "implementations",
    "api",
    "flow",
    "docs",
    "test",
    "config",
    "error",
    "similar",
];
const WORKFLOW: &[&str] = &[
    "review",
    "refactor",
    "doc-sync",
    "divergence",
    "onboard",
    "security",
];

#[test]
fn the_shipped_library_holds_every_planned_intent_of_the_right_kind() {
    let library = builtin();
    for name in LOCATE {
        let intent = library.get(name).expect("shipped");
        assert_eq!(intent.kind, IntentKind::Locate, "{name}");
        assert_eq!(intent.legs.len(), 1, "{name}: a locate intent has one leg");
    }
    for name in WORKFLOW {
        let intent = library.get(name).expect("shipped");
        assert_eq!(intent.kind, IntentKind::Workflow, "{name}");
        assert!(
            intent.legs.len() >= 2,
            "{name}: a workflow runs several legs"
        );
    }
    assert_eq!(library.iter().count(), LOCATE.len() + WORKFLOW.len());
    let doc_sync = library.get("doc-sync").expect("shipped");
    let divergence = library.get("divergence").expect("shipped");
    assert!(doc_sync.pairing && !doc_sync.duplicates);
    assert!(divergence.pairing && divergence.duplicates);
}

/// The embedding-model convention: one sentence stating the input, then what
/// to retrieve — for the intent and every leg it runs.
#[test]
fn every_instruction_states_the_input_then_what_to_retrieve_and_names_no_language() {
    for intent in builtin().iter() {
        let own = std::iter::once((intent.name.clone(), intent.instruction()));
        let legs = intent
            .legs
            .iter()
            .map(|l| (format!("{}.{}", intent.name, l.label), l.instruction()));
        for (at, template) in own.chain(legs) {
            let neutral = template.sentence(&[]);
            assert!(neutral.starts_with("Given "), "{at}: {neutral}");
            assert!(neutral.contains(", retrieve "), "{at}: {neutral}");
            assert!(neutral.ends_with('.'), "{at}: {neutral}");
            assert!(!neutral.contains(". "), "{at}: one sentence: {neutral}");
            let lower = neutral.to_ascii_lowercase();
            for lang in Lang::ALL.iter().filter(|l| **l != Lang::Other) {
                let name = lang.as_str();
                assert!(
                    !lower.contains(&format!(" {name} ")),
                    "{at}: names `{name}` outside a slot: {neutral}"
                );
            }
        }
    }
}

#[test]
fn the_default_intent_renders_the_default_instruction() {
    let code = builtin().get("code").expect("shipped");
    assert_eq!(code.neutral_instruction(), DEFAULT_QUERY_INSTRUCTION);
}

#[test]
fn workflow_legs_borrow_the_locate_intents_they_name() {
    let review = builtin().get("review").expect("shipped");
    let borrowed: Vec<_> = review.legs.iter().map(|l| l.borrows.as_deref()).collect();
    assert_eq!(
        borrowed,
        [Some("usage"), Some("test"), Some("docs"), Some("similar")]
    );
    let usage = builtin().get("usage").expect("shipped");
    assert_eq!(review.legs[0].instruction(), usage.instruction());

    let doc_sync = builtin().get("doc-sync").expect("shipped");
    assert_eq!(doc_sync.legs[0].langs, vec![Lang::Markdown]);
    assert_eq!(doc_sync.legs[0].kinds, vec![ChunkKind::DocSection]);
    assert_eq!(doc_sync.legs[1].exclude_langs, vec![Lang::Markdown]);
}

#[test]
fn an_unknown_intent_is_refused_with_the_list_of_known_ones() {
    let err = builtin().get("telepathy").expect_err("unknown");
    assert_eq!(err.code(), "invalid");
    let text = err.to_string();
    assert!(
        text.contains("telepathy") && text.contains("usage") && text.contains("doc-sync"),
        "{text}"
    );
}

#[test]
fn names_and_aliases_are_looked_up_case_and_space_insensitively() {
    assert_eq!(builtin().get(" Callers ").expect("alias").name, "usage");
    assert_eq!(builtin().get("DRIFT").expect("alias").name, "doc-sync");
}

// --- golden renderings: a data change shows up as a readable diff ------------

#[test]
fn usage_for_rust_axle_and_php_renders_one_sentence_with_their_hints() {
    let usage = builtin().get("usage").expect("shipped");
    let langs = [Lang::Rust, Lang::Axle, Lang::Php];
    let out = usage.instruction_for(&langs, DEFAULT_MAX_INSTRUCTION_CHARS);
    assert_eq!(
        out,
        "Given an API, function or type of a Rust, Axle and PHP codebase, retrieve the passages \
         that call, instantiate, import or depend on it, showing how it is used in practice; \
         Rust: call sites, `impl Trait for` blocks, generic bounds, macro invocations and `use` \
         paths; Axle: calls passing `own`, `mut` or `copy`, `class Foo : Base, Trait`, \
         `impl Type : Trait`, `when T : Bound` groups and `use crate::…`; PHP: `new X`, `X::` \
         static calls, `use` imports, `implements`, `extends` and DI container wiring."
    );
    assert!(out.len() <= DEFAULT_MAX_INSTRUCTION_CHARS);
}

#[test]
fn error_for_go_alone_names_go_and_carries_its_idioms() {
    let error = builtin().get("diagnostic").expect("alias");
    let out = error.instruction_for(&[Lang::Go], DEFAULT_MAX_INSTRUCTION_CHARS);
    assert_eq!(
        out,
        "Given an error message, a panic, an exception or a diagnostic code from a Go codebase, \
         retrieve the code that raises it, the code that propagates or handles it, and the type \
         that defines it; Go: `if err != nil` returns, `errors.Is` and `errors.As`, \
         `fmt.Errorf(\"%w\")` and sentinel errors."
    );
}

#[test]
fn docs_for_axle_says_an_axle_project() {
    let docs = builtin().get("docs").expect("shipped");
    let out = docs.instruction_for(&[Lang::Axle], DEFAULT_MAX_INSTRUCTION_CHARS);
    assert!(
        out.starts_with("Given a question about an Axle project, "),
        "{out}"
    );
}

#[test]
fn many_languages_keep_the_whole_sentence_and_drop_hints_past_the_cap() {
    let test = builtin().get("test").expect("shipped");
    let langs = [
        Lang::Rust,
        Lang::Go,
        Lang::Python,
        Lang::JavaScript,
        Lang::Java,
        Lang::Kotlin,
        Lang::Php,
    ];
    let out = test.instruction_for(&langs, DEFAULT_MAX_INSTRUCTION_CHARS);
    let sentence = test.instruction().sentence(&langs);
    assert!(out.len() <= DEFAULT_MAX_INSTRUCTION_CHARS, "{}", out.len());
    assert!(out.starts_with(sentence.trim_end_matches('.')), "{out}");
    assert!(out.contains("; Rust: ") && out.contains("; Go: "), "{out}");
    assert!(
        !out.contains("; PHP: "),
        "the lowest priority goes first: {out}"
    );
    assert!(out.ends_with('.'));
}

// --- layers -------------------------------------------------------------------

#[test]
fn a_manifest_override_beats_the_daemons_which_beats_the_builtin() {
    let daemon = builtin()
        .merged(&overrides(
            "[usage]\ninstruction = \"Given an API, retrieve what the daemon wants.\"\n\
             [usage.hints]\nrust = [\"daemon rust\"]\ngo = [\"daemon go\"]\n",
        ))
        .expect("daemon layer");
    let project = daemon
        .merged(&overrides(
            "[callers]\ninstruction = \"Given an API, retrieve what the project wants.\"\n\
             [callers.hints]\nrust = [\"project rust\", \"its macros\"]\n",
        ))
        .expect("project layer, keyed by an alias");
    let usage = project.get("usage").expect("still there");
    assert_eq!(
        usage.instruction_for(&[Lang::Rust, Lang::Go], 1_000),
        "Given an API, retrieve what the project wants; Rust: project rust and its macros; Go: \
         daemon go."
    );
    let below = daemon.get("usage").expect("there").neutral_instruction();
    assert_eq!(
        below, "Given an API, retrieve what the daemon wants.",
        "a layer never edits the library below it"
    );
}

#[test]
fn an_override_of_a_locate_intent_reaches_the_workflow_leg_that_borrows_it() {
    let library = builtin()
        .merged(&overrides(
            "[test]\ninstruction = \"Given a behaviour, retrieve its specs.\"\n",
        ))
        .expect("merged");
    let review = library.get("review").expect("there");
    let tests = review
        .legs
        .iter()
        .find(|l| l.label == "tests")
        .expect("leg");
    assert_eq!(
        tests.instruction().source(),
        "Given a behaviour, retrieve its specs."
    );
}

#[test]
fn a_broken_layer_is_refused_with_what_is_wrong() {
    let cases = [
        ("[usage.hints]\ncobol = [\"x\"]\n", "not a language name"),
        ("[usage.hints]\nother = [\"x\"]\n", "not a language name"),
        (
            "[usage.hints]\nrust = [\"ends with a period.\"]\n",
            "no closing",
        ),
        ("[usage.hints]\nrust = [\" \"]\n", "short phrase"),
        ("[symbol]\naliases = [\"callers\"]\n", "names both"),
        (
            "[review]\n[[review.legs]]\nlabel = \"x\"\nintent = \"nope\"\n",
            "names no locate intent",
        ),
        (
            "[review]\n[[review.legs]]\nlabel = \"x\"\nintent = \"refactor\"\n",
            "names no locate intent",
        ),
        (
            "[usage]\ninstruction = \"Given {langs} code, retrieve it.\"\n",
            "{codebase}",
        ),
        (
            "[usage]\nkind = \"workflow\"\n[[usage.legs]]\nlabel = \"a\"\n[[usage.legs]]\nlabel = \"a\"\n",
            "two legs",
        ),
        ("[usage]\npairing = true\n", "pairing"),
        ("[usage]\nduplicates = true\n", "workflow"),
        (
            "[review]\n[[review.legs]]\nlabel = \"x\"\nlangs = [\"rust\"]\nexclude_langs = [\"go\"]\n",
            "keep one",
        ),
        (
            "[Bad_Name]\nsummary = \"s\"\ninstruction = \"Given x, retrieve y.\"\n",
            "lowercase",
        ),
    ];
    for (src, needle) in cases {
        let err = builtin().merged(&overrides(src)).expect_err(src);
        assert_eq!(err.code(), "config", "{src}");
        assert!(err.to_string().contains(needle), "{src}: {err}");
    }
}

#[test]
fn a_new_intent_from_a_layer_is_listed_and_resolvable() {
    let library = builtin()
        .merged(&overrides(
            "[migration]\naliases = [\"schema\"]\nsummary = \"Schema migrations.\"\n\
             instruction = \"Given a schema change in {codebase}, retrieve the migrations that \
             apply it.\"\n[migration.hints]\nphp = [\"Doctrine migration classes\"]\n",
        ))
        .expect("merged");
    assert!(library.names().any(|n| n == "migration"));
    let ctx = PlanContext {
        library: &library,
        scope: &SearchFilter::default(),
        project_instruction: None,
        project_intent: None,
        langs: &[Lang::Php],
        max_instruction_chars: DEFAULT_MAX_INSTRUCTION_CHARS,
    };
    let plan = SearchRequest::new(ProjectId::new("demo").expect("id"), "q")
        .with_intent("schema")
        .resolve_plan(&ctx)
        .expect("resolves");
    assert_eq!(
        plan.legs[0].instruction.as_deref(),
        Some(
            "Given a schema change in a PHP codebase, retrieve the migrations that apply it; \
             PHP: Doctrine migration classes."
        )
    );
}
