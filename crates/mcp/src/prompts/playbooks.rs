//! The playbooks: for each task, the findery calls to make in order, what to
//! verify, and which intents' language hints make its checklist.
//!
//! INTENT: data, not code. A step is a sentence with two slots — `{target}`
//! and `{narrow}` (the scope as `, scope: "api"` or `, paths: ["src/**"]`) —
//! so a playbook reads here exactly as the model will read it.

use findery_core::model::Lang;

/// One task an agent can be handed.
pub struct Playbook {
    pub name: &'static str,
    /// The workflow intent it starts from; its summary describes the prompt.
    pub intent: Option<&'static str>,
    /// Used when no intent describes it.
    pub summary: &'static str,
    pub title: &'static str,
    /// What `target` names; `None` makes it optional.
    pub target: Option<&'static str>,
    pub steps: &'static [&'static str],
    pub verify: &'static [&'static str],
    /// The intents whose per-language hints make the checklist.
    pub hints_from: &'static [&'static str],
    /// Whether the checklist warns about what a change breaks in each language.
    pub caveats: bool,
}

/// Every playbook, in the order `prompts/list` gives them.
pub const PLAYBOOKS: &[Playbook] = &[
    Playbook {
        name: "review",
        intent: Some("review"),
        summary: "",
        title: "Review",
        target: Some("the function, type or module under review"),
        steps: &[
            "search {query: \"{target}\", intent: \"symbol\"{narrow}} — its definition; then read {locator: <that hit>, context: 30} for all of it.",
            "search {query: \"{target}\", intent: \"review\"{narrow}} — callers, tests, docs and similar code, one section each.",
            "search {query: \"{target}\", mode: \"keyword\", k: 30} — exact call sites the semantic legs missed (re-exports, string-built names).",
            "read every caller and test the change can break, with enough context to see the call.",
        ],
        verify: &[
            "each caller still holds: arguments, error handling, ownership, return value;",
            "a test asserts the new behaviour, and none still asserts the old one;",
            "the `## docs` hits still say what the code does;",
            "each `## similar` hit: should it change the same way?",
        ],
        hints_from: &["usage", "test", "error"],
        caveats: true,
    },
    Playbook {
        name: "refactor",
        intent: Some("refactor"),
        summary: "",
        title: "Refactor",
        target: Some("the symbol to rename, move or reshape"),
        steps: &[
            "search {query: \"{target}\", intent: \"refactor\"{narrow}, detail: \"full\"} — definition, usages, implementations, tests, duplicates.",
            "search {query: \"{target}\", mode: \"keyword\", k: 50} — every textual occurrence, docs, config and strings included.",
            "read {locator: <the definition>, context: 20}, then each implementation.",
            "after editing: sync {paths: [<the files you changed>]}, then search the old name again — nothing may be left.",
        ],
        verify: &[
            "the change list covers every usage and implementation found, in code and in docs or config;",
            "tests pin the behaviour that must not change, and they pass after;",
            "the `## duplicates` hits change together, or you say why not.",
        ],
        hints_from: &["symbol", "usage", "implementations", "test"],
        caveats: true,
    },
    Playbook {
        name: "doc-sync",
        intent: Some("doc-sync"),
        summary: "",
        title: "Check docs against code",
        target: None,
        steps: &[
            "search {query: \"{target}\", intent: \"doc-sync\"{narrow}} — each doc section beside its nearest code (`## pairs`).",
            "for each `possibly drifted` pair: read both locators and compare options, defaults, limits and error behaviour.",
            "for each default or setting a doc states: search {query: <it>, intent: \"config\"}.",
        ],
        verify: &[
            "every documented option, default and limit exists in code with that value;",
            "every `no counterpart found` section: removed, renamed, or never built?",
            "report drift as doc locator ↔ code locator, saying which side is right.",
        ],
        hints_from: &["docs", "config"],
        caveats: false,
    },
    Playbook {
        name: "divergence",
        intent: Some("divergence"),
        summary: "",
        title: "Find divergence",
        target: None,
        steps: &[
            "search {query: \"{target}\", intent: \"divergence\"{narrow}} — doc claims, the code that should honour them, near-duplicates.",
            "read both sides of every `≈` near-duplicate: list the lines that differ and whether the difference is intended.",
            "for each drifted claim: search {query: <the claim>, intent: \"code\"} before calling it unimplemented.",
        ],
        verify: &[
            "a copy that diverged by accident is a bug in one of the two: name which;",
            "a doc claim with no code is either a missing feature or a stale doc: say which.",
        ],
        hints_from: &["docs", "config"],
        caveats: false,
    },
    Playbook {
        name: "onboard",
        intent: Some("onboard"),
        summary: "",
        title: "Onboard",
        target: None,
        steps: &[
            "search {query: \"{target}\", intent: \"onboard\"{narrow}, detail: \"full\"} — overview docs, entry points, central types.",
            "read {glob: <the area's directory>/**} — the files that make it up.",
            "for each central type: search {query: <it>, intent: \"usage\"}; for the main operation: search {query: <it>, intent: \"flow\"}.",
        ],
        verify: &[
            "deliver a map: entry points, central types, the main data flow, where the tests live, what to read first — each as a locator.",
        ],
        hints_from: &["onboard", "api"],
        caveats: false,
    },
    Playbook {
        name: "security",
        intent: Some("security"),
        summary: "",
        title: "Security review",
        target: None,
        steps: &[
            "search {query: \"{target}\", intent: \"security\"{narrow}, detail: \"full\"} — input, auth, unsafe, injection and secrets, one section each.",
            "for each input point: search {query: <the input>, intent: \"flow\"} to follow it to its sink, then read the sink.",
            "search {query: \"api key token password secret\", intent: \"config\"}, then mode: \"keyword\" for literal key names.",
        ],
        verify: &[
            "untrusted data is validated before its first side effect;",
            "no query, command, path or URL is concatenated from input;",
            "every entry point checks who calls it;",
            "secrets are never logged, echoed or committed; each unsafe or FFI block says why it is sound.",
        ],
        hints_from: &["security", "config"],
        caveats: true,
    },
    Playbook {
        name: "bug-trace",
        intent: None,
        summary: "From a symptom to its root cause: where the error is raised, the path that reaches it, and the tests that should have caught it.",
        title: "Trace a bug",
        target: Some("the symptom: an error message, a failing test, a wrong output"),
        steps: &[
            "search {query: \"{target}\", intent: \"error\", mode: \"keyword\"} — where the exact message is raised.",
            "search {query: \"{target}\", intent: \"error\"{narrow}} — the code that raises, propagates and handles it.",
            "search {query: <the operation that fails>, intent: \"flow\"{narrow}} — entry point to sink; read each hop with context: 15.",
            "search {query: <the behaviour>, intent: \"test\"} — the tests that should have caught it.",
        ],
        verify: &[
            "name the root cause at a locator, not the place the symptom shows;",
            "measure the fix's reach: search {query: <what you change>, intent: \"usage\"};",
            "add a test that fails before the fix and passes after.",
        ],
        hints_from: &["error", "flow", "test"],
        caveats: true,
    },
];

/// What a change breaks in each language, in one sentence.
pub fn caveat(lang: Lang) -> Option<&'static str> {
    Some(match lang {
        Lang::Rust => {
            "a `pub` signature or trait change breaks every caller and every `impl`; `?` changes which error reaches the caller; code under `#[cfg(test)]` or a feature flag compiles only there."
        }
        Lang::Axle => {
            "an error added to a signature's `! E` set must be caught or re-declared by every caller; `own` moves the value and `mut` borrows it exclusively, so a changed call site can stop compiling; `when T : Bound` methods exist only for those `T`."
        }
        Lang::Php => {
            "types are checked at run time only: a changed signature fails on the path that reaches it, so search string-built calls (`$obj->$name()`, container ids) too."
        }
        Lang::JavaScript => {
            "a re-export or barrel `index` hides the real importer; TypeScript types vanish at run time, so a changed shape breaks only where the data is read."
        }
        Lang::Python => {
            "duck typing: a renamed attribute breaks call sites no import names — search it with mode: \"keyword\"; decorators and `__init__.py` re-exports change what a name resolves to."
        }
        Lang::Java => {
            "a changed interface method breaks every `implements`; annotations (`@Bean`, `@Autowired`) wire classes no call site names; checked `throws` reach every caller."
        }
        Lang::Kotlin => {
            "`override` and `: Interface` tie implementations to their base; extension functions are called on types that never declare them; `suspend` changes every caller's context."
        }
        Lang::Go => {
            "interfaces are satisfied implicitly — search by method set, not by interface name; a changed error value breaks `errors.Is` checks; `_test.go` files sit beside the code."
        }
        Lang::Markdown | Lang::Toml | Lang::Json | Lang::Other => return None,
    })
}

/// The playbook `name` names.
pub fn find(name: &str) -> Option<&'static Playbook> {
    PLAYBOOKS.iter().find(|p| p.name == name)
}
