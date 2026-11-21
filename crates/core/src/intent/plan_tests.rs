//! The ladder, the filter narrowing and `like`, against the shipped library.

use std::collections::BTreeMap;

use super::*;
use crate::ids::ProjectId;
use crate::intent::{DEFAULT_MAX_INSTRUCTION_CHARS, IntentOverride, QueryTask};

fn builtin() -> &'static IntentLibrary {
    IntentLibrary::builtin().expect("the shipped library loads")
}

fn request() -> SearchRequest {
    SearchRequest::new(ProjectId::new("demo").expect("id"), "q")
}

fn ctx<'a>(library: &'a IntentLibrary, scope: &'a SearchFilter) -> PlanContext<'a> {
    PlanContext {
        library,
        scope,
        project_instruction: None,
        project_intent: None,
        langs: &[],
        max_instruction_chars: DEFAULT_MAX_INSTRUCTION_CHARS,
    }
}

fn plan(req: &SearchRequest) -> ResolvedPlan {
    req.resolve_plan(&ctx(builtin(), &SearchFilter::default()))
        .expect("resolves")
}

fn neutral(name: &str) -> String {
    let intent = builtin().get(name).expect("shipped");
    intent.neutral_instruction().to_string()
}

/// What the request says, what the project says, and what the single leg
/// must carry.
struct Row {
    raw: bool,
    instruction: Option<&'static str>,
    intent: Option<&'static str>,
    project_instruction: Option<&'static str>,
    project_intent: Option<&'static str>,
    expect: Option<String>,
    expect_intent: Option<&'static str>,
}

const FREE: &str = "Given the request, retrieve this.";
const PROJECT: &str = "Given the project, retrieve that.";

fn ladder() -> Vec<Row> {
    let quiet = Row {
        raw: false,
        instruction: None,
        intent: None,
        project_instruction: None,
        project_intent: None,
        expect: None,
        expect_intent: None,
    };
    vec![
        // raw beats everything, and sends no instruction at all.
        Row {
            raw: true,
            instruction: Some(FREE),
            intent: Some("symbol"),
            project_instruction: Some(PROJECT),
            expect: None,
            expect_intent: Some("symbol"),
            ..quiet
        },
        // a free instruction beats the intent's sentence.
        Row {
            instruction: Some(FREE),
            intent: Some("symbol"),
            project_instruction: Some(PROJECT),
            expect: Some(FREE.into()),
            expect_intent: Some("symbol"),
            ..quiet
        },
        // the request's intent beats the project's sentence.
        Row {
            intent: Some("symbol"),
            project_instruction: Some(PROJECT),
            project_intent: Some("usage"),
            expect: Some(neutral("symbol")),
            expect_intent: Some("symbol"),
            ..quiet
        },
        // the project's sentence beats the project's intent.
        Row {
            project_instruction: Some(PROJECT),
            project_intent: Some("usage"),
            expect: Some(PROJECT.into()),
            ..quiet
        },
        // the project's intent, when nothing above speaks.
        Row {
            project_intent: Some("callers"),
            expect: Some(neutral("usage")),
            expect_intent: Some("usage"),
            ..quiet
        },
        // a blank request instruction says nothing.
        Row {
            instruction: Some("   "),
            project_instruction: Some(PROJECT),
            expect: Some(PROJECT.into()),
            ..quiet
        },
        // nothing at all: the embedder's own default.
        quiet,
    ]
}

#[test]
fn the_precedence_ladder_is_resolved_in_one_place() {
    let scope = SearchFilter::default();
    for (n, row) in ladder().iter().enumerate() {
        let mut req = request();
        req.raw_query = row.raw;
        req.instruction = row.instruction.map(str::to_string);
        req.intent = row.intent.map(str::to_string);
        let context = PlanContext {
            project_instruction: row.project_instruction,
            project_intent: row.project_intent,
            ..ctx(builtin(), &scope)
        };
        let plan = req.resolve_plan(&context).expect("resolves");
        assert_eq!(plan.legs.len(), 1, "row {n}");
        assert_eq!(plan.legs[0].instruction, row.expect, "row {n}");
        assert_eq!(plan.legs[0].raw, row.raw, "row {n}");
        assert_eq!(plan.intent.as_deref(), row.expect_intent, "row {n}");
        assert!(!plan.workflow, "row {n}: a locate search is one list");
    }
}

#[test]
fn each_leg_says_how_its_query_is_prompted() {
    let mut req = request();
    assert_eq!(plan(&req).legs[0].prompt(), QueryPrompt::Default);
    req.instruction = Some(FREE.into());
    assert_eq!(plan(&req).legs[0].prompt(), QueryPrompt::Instruct(FREE));
    req.raw_query = true;
    assert_eq!(plan(&req).legs[0].prompt(), QueryPrompt::Raw);
}

#[test]
fn a_raw_query_keeps_the_intents_legs_but_sends_no_instruction() {
    let mut req = request().with_intent("review");
    req.raw_query = true;
    let plan = plan(&req);
    assert_eq!(plan.legs.len(), 4);
    assert!(plan.workflow);
    assert!(plan.legs.iter().all(|l| l.raw && l.instruction.is_none()));
}

#[test]
fn the_legacy_task_and_the_intent_mean_the_same_and_may_not_disagree() {
    let mut both = request().with_task(QueryTask::Usage).with_intent("callers");
    assert_eq!(plan(&both).intent.as_deref(), Some("usage"));

    both.intent = Some("symbol".into());
    let err = both
        .resolve_plan(&ctx(builtin(), &SearchFilter::default()))
        .expect_err("two intents");
    assert_eq!(err.code(), "invalid");
}

#[test]
fn a_workflow_leg_keeps_its_own_k_unless_the_request_names_one() {
    let plan_default = plan(&request().with_intent("review"));
    assert_eq!(plan_default.legs[0].label, "callers");
    assert_eq!(plan_default.legs[0].k, Some(8));

    let mut req = request().with_intent("review");
    req.k = Some(2);
    assert!(plan(&req).legs.iter().all(|l| l.k == Some(2)));
}

// --- like ---------------------------------------------------------------------

#[test]
fn like_alone_runs_the_similar_intent_by_example_and_embeds_nothing() {
    let mut req = request();
    req.like = Some("src/lib.rs:L1-L9".into());
    let plan = plan(&req);
    assert_eq!(plan.intent.as_deref(), Some(LIKE_INTENT));
    assert!(plan.legs[0].by_example);
    assert_eq!(plan.legs[0].instruction, None);
}

#[test]
fn like_feeds_only_the_similar_leg_of_a_workflow() {
    let mut req = request().with_intent("review");
    req.like = Some("src/lib.rs:L1-L9".into());
    let plan = plan(&req);
    let example: Vec<&str> = plan
        .legs
        .iter()
        .filter(|l| l.by_example)
        .map(|l| l.label.as_str())
        .collect();
    assert_eq!(example, ["similar"]);
    assert!(plan.legs[0].instruction.is_some(), "callers still embed");
}

#[test]
fn like_with_an_intent_that_has_no_similar_leg_is_refused() {
    let mut req = request().with_intent("usage");
    req.like = Some("src/lib.rs:L1-L9".into());
    let err = req
        .resolve_plan(&ctx(builtin(), &SearchFilter::default()))
        .expect_err("nothing takes the example");
    assert_eq!(err.code(), "invalid");
    assert!(err.to_string().contains("similar"), "{err}");
}

// --- filters ------------------------------------------------------------------

#[test]
fn a_leg_narrows_the_request_filter_and_never_widens_it() {
    let scope = SearchFilter {
        paths: vec!["docs/**".into(), "src/**".into()],
        ..SearchFilter::default()
    };
    let mut req = request().with_intent("doc-sync");
    req.filter.langs = vec![Lang::Rust, Lang::Markdown];
    let plan = req.resolve_plan(&ctx(builtin(), &scope)).expect("resolves");
    let (docs, code) = (&plan.legs[0], &plan.legs[1]);
    assert_eq!(docs.filter.langs, vec![Lang::Markdown], "intersected");
    assert_eq!(docs.filter.kinds, vec![ChunkKind::DocSection]);
    assert_eq!(code.filter.langs, vec![Lang::Rust], "markdown subtracted");
    assert_eq!(docs.filter.paths, scope.paths, "the scope's paths survive");
    assert!(plan.pairing && plan.warnings.is_empty());
}

#[test]
fn an_exclusion_with_no_language_named_keeps_every_other_language() {
    let plan = plan(&request().with_intent("doc-sync"));
    let code = &plan.legs[1].filter.langs;
    assert!(!code.contains(&Lang::Markdown));
    assert_eq!(code.len(), Lang::ALL.len() - 1);
}

#[test]
fn a_leg_the_filter_empties_is_dropped_and_reported_and_pairing_is_lifted() {
    let mut req = request().with_intent("doc-sync");
    req.filter.langs = vec![Lang::Rust];
    let plan = plan(&req);
    assert_eq!(plan.legs.len(), 1);
    assert_eq!(plan.legs[0].label, "code");
    assert!(plan.workflow && !plan.pairing);
    let reported = plan
        .warnings
        .iter()
        .any(|w| w.contains("`docs`") && w.contains("markdown"));
    assert!(reported, "{:?}", plan.warnings);
}

#[test]
fn a_filter_that_empties_every_leg_is_refused() {
    let layer: BTreeMap<String, IntentOverride> = toml::from_str(
        "[prose]\nsummary = \"Prose only.\"\n\
         instruction = \"Given a topic, retrieve the prose about it.\"\n\
         [[prose.legs]]\nlabel = \"prose\"\nlangs = [\"markdown\"]\n",
    )
    .expect("parses");
    let library = builtin().merged(&layer).expect("merged");
    let mut req = request().with_intent("prose");
    req.filter.langs = vec![Lang::Go];
    let err = req
        .resolve_plan(&ctx(&library, &SearchFilter::default()))
        .expect_err("nothing to search");
    assert_eq!(err.code(), "invalid");
    assert!(err.to_string().contains("prose"), "{err}");
}

#[test]
fn the_instruction_names_the_projects_languages_when_the_filter_names_none() {
    let scope = SearchFilter::default();
    let langs = [Lang::Rust, Lang::Markdown, Lang::Php];
    let context = PlanContext {
        langs: &langs,
        ..ctx(builtin(), &scope)
    };
    let plan = request()
        .with_intent("doc-sync")
        .resolve_plan(&context)
        .expect("resolves");
    let docs = plan.legs[0].instruction.as_deref().expect("rendered");
    let code = plan.legs[1].instruction.as_deref().expect("rendered");
    assert!(docs.contains("; Markdown: "), "{docs}");
    assert!(code.contains("a Rust and PHP codebase"), "{code}");
    assert!(
        !code.contains("Markdown"),
        "the code leg excludes it: {code}"
    );
}
