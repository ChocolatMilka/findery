//! End-to-end chunking of a realistic file per language, through the registry.
//!
//! INTENT: the per-module tests pin down one rule each. This one answers the
//! question a user actually asks — "what does my project look like once it is
//! indexed?" — and it is where a language proves it produces named items
//! rather than one anonymous blob.

mod fixtures;

use findery_chunk::Registry;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use fixtures::*;

fn chunks_under(path: &str, src: &str, max_chars: usize) -> Vec<RawChunk> {
    let cfg = ChunkConfig {
        min_chars: 1,
        max_chars,
        ..ChunkConfig::default()
    };
    Registry::new()
        .chunk(path, Lang::from_path(path), src, &cfg)
        .expect("chunking succeeds")
}

fn chunks(path: &str, src: &str) -> Vec<RawChunk> {
    chunks_under(path, src, ChunkConfig::default().max_chars)
}

fn names(out: &[RawChunk]) -> Vec<String> {
    out.iter()
        .filter_map(|c| c.symbol.clone())
        .collect::<Vec<_>>()
}

fn named<'a>(out: &'a [RawChunk], symbol: &str) -> &'a RawChunk {
    out.iter()
        .find(|c| c.symbol.as_deref() == Some(symbol))
        .unwrap_or_else(|| panic!("no chunk named `{symbol}` in {out:#?}"))
}

#[test]
fn a_javascript_module_yields_one_chunk_per_declaration() {
    let out = chunks("src/client.js", JS_SRC);
    assert_eq!(
        names(&out),
        vec!["DEFAULTS", "withBackoff", "Client"],
        "got {out:#?}"
    );
    let client = named(&out, "Client");
    assert_eq!(client.kind, ChunkKind::Struct);
    assert_eq!(client.start_line, 16);
    let backoff = named(&out, "withBackoff");
    assert_eq!(backoff.kind, ChunkKind::Function);
    assert_eq!(backoff.start_line, 5); // the comment rides with it
    assert_eq!(backoff.end_line, 14);
}

#[test]
fn a_typescript_module_keeps_its_type_declarations() {
    let out = chunks("src/retry.ts", TS_SRC);
    assert_eq!(
        names(&out),
        vec!["RetryOptions", "Handler", "MAX_ATTEMPTS", "retry"],
        "got {out:#?}"
    );
    assert_eq!(named(&out, "RetryOptions").kind, ChunkKind::Trait);
    assert_eq!(named(&out, "MAX_ATTEMPTS").kind, ChunkKind::Const);
}

#[test]
fn a_python_module_yields_one_chunk_per_definition() {
    let out = chunks("app/limits.py", PYTHON_SRC);
    assert_eq!(
        names(&out),
        vec!["limits", "DEFAULT_WINDOW", "sleep_backoff", "Limiter"],
        "got {out:#?}"
    );
    assert_eq!(out[0].kind, ChunkKind::Module);
    assert_eq!(named(&out, "DEFAULT_WINDOW").kind, ChunkKind::Const);
    assert_eq!(named(&out, "sleep_backoff").kind, ChunkKind::Function);
    let limiter = named(&out, "Limiter");
    assert_eq!(limiter.kind, ChunkKind::Struct);
    assert_eq!(limiter.start_line, 13);
    assert_eq!(limiter.end_line, 20);
}

#[test]
fn a_php_class_yields_one_chunk_until_it_has_to_split() {
    // A class that fits the budget stays whole, methods included.
    let out = chunks("src/Http/Client.php", PHP_SRC);
    assert_eq!(names(&out), vec!["App\\Http", "Client"], "got {out:#?}");
    let client = named(&out, "Client");
    assert_eq!(client.kind, ChunkKind::Struct);
    assert!(client.text.contains("TIMEOUT"));
    assert!(client.text.contains("function send"));

    // Under pressure it splits per member, each with its breadcrumb.
    let tight = chunks_under("src/Http/Client.php", PHP_SRC, 200);
    let send = named(&tight, "send");
    assert_eq!(send.kind, ChunkKind::Method);
    assert_eq!(send.breadcrumb.as_deref(), Some("Client > send"));
    assert_eq!(named(&tight, "TIMEOUT").kind, ChunkKind::Const);
    assert!(!tight.iter().any(|c| c.text.contains("<?php")));
}

#[test]
fn a_json_document_answers_whole_until_it_is_too_big() {
    let src =
        "{\n  \"dependencies\": {\n    \"a\": \"1.0.0\"\n  },\n  \"packageManager\": \"pnpm\"\n}\n";
    let whole = chunks("package.json", src);
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].kind, ChunkKind::File);
    assert!(whole[0].text.contains("packageManager"));

    let split = chunks_under("package.json", src, 60);
    assert_eq!(
        names(&split),
        vec!["dependencies", "packageManager"],
        "got {split:#?}"
    );
}

#[test]
fn a_rust_module_yields_one_chunk_per_item() {
    let out = chunks("src/backoff.rs", RUST_SRC);
    assert_eq!(
        names(&out),
        vec![
            "backoff",
            "MAX_ATTEMPTS",
            "with_backoff",
            "Policy",
            "Policy"
        ],
        "got {out:#?}"
    );
    assert_eq!(out[0].kind, ChunkKind::Module);
    assert_eq!(named(&out, "MAX_ATTEMPTS").kind, ChunkKind::Const);
    assert_eq!(named(&out, "Policy").kind, ChunkKind::Struct);
    assert!(out.iter().any(|c| c.kind == ChunkKind::Impl));
}

#[test]
fn a_java_class_yields_one_chunk_until_it_has_to_split() {
    let out = chunks("src/main/java/app/http/Client.java", JAVA_SRC);
    assert_eq!(names(&out), vec!["Client", "Audited"], "got {out:#?}");
    let client = named(&out, "Client");
    assert_eq!(client.kind, ChunkKind::Struct);
    assert_eq!(client.start_line, 6); // the javadoc rides with it
    assert!(client.text.contains("@Service"));
    assert_eq!(named(&out, "Audited").kind, ChunkKind::Trait);

    let tight = chunks_under("src/main/java/app/http/Client.java", JAVA_SRC, 300);
    // Split, the class keeps a chunk of its own: the header its members lack.
    let header = named(&tight, "Client");
    assert_eq!((header.start_line, header.end_line), (6, 8));
    assert_eq!(named(&tight, "MAX_RETRIES").kind, ChunkKind::Const);
    let send = named(&tight, "send");
    assert_eq!(send.kind, ChunkKind::Method);
    assert_eq!(send.breadcrumb.as_deref(), Some("Client > send"));
    assert_eq!((send.start_line, send.end_line), (17, 25));
}

#[test]
fn a_kotlin_file_yields_one_chunk_per_declaration_without_semicolons() {
    let out = chunks("src/main/kotlin/app/http/Client.kt", KOTLIN_SRC);
    assert_eq!(
        names(&out),
        vec!["MAX_RETRIES", "Client", "String.slug", "sends once"],
        "got {out:#?}"
    );
    assert_eq!(named(&out, "MAX_RETRIES").kind, ChunkKind::Const);
    let client = named(&out, "Client");
    assert_eq!((client.start_line, client.end_line), (8, 18));
    assert_eq!(named(&out, "sends once").kind, ChunkKind::Test);

    let tight = chunks_under("src/main/kotlin/app/http/Client.kt", KOTLIN_SRC, 150);
    let send = named(&tight, "send");
    assert_eq!(send.kind, ChunkKind::Method);
    assert_eq!(send.breadcrumb.as_deref(), Some("Client > send"));
    assert_eq!(named(&tight, "Companion").kind, ChunkKind::Struct);
}

#[test]
fn a_go_file_yields_functions_methods_and_types_with_their_comments() {
    let out = chunks("client/client_test.go", GO_SRC);
    assert_eq!(
        names(&out),
        vec!["MaxRetries", "Client", "Client.Send", "TestSend"],
        "got {out:#?}"
    );
    assert!(
        out[0].symbol.is_none(),
        "package and imports stay anonymous"
    );
    let send = named(&out, "Client.Send");
    assert_eq!(send.kind, ChunkKind::Method);
    assert_eq!((send.start_line, send.end_line), (16, 21));
    assert_eq!(named(&out, "Client").kind, ChunkKind::Struct);
    assert_eq!(named(&out, "TestSend").kind, ChunkKind::Test);
    assert_eq!(named(&out, "MaxRetries").start_line, 9);
}

#[test]
fn an_axle_module_yields_named_items_with_their_annotations_and_docs() {
    let out = chunks("std/buf.axle", AXLE_SRC);
    assert_eq!(named(&out, "Sort").kind, ChunkKind::Enum);
    assert!(
        named(&out, "Sort")
            .text
            .starts_with("/** Sort direction. */")
    );
    let ext = named(&out, "libc_getpid");
    assert_eq!(ext.kind, ChunkKind::Function);
    assert!(ext.text.starts_with("@link("));
    assert_eq!(named(&out, "Buf").kind, ChunkKind::Struct);

    let tight = chunks_under("std/buf.axle", AXLE_SRC, 90);
    let cap = named(&tight, "cap");
    assert_eq!(cap.kind, ChunkKind::Method);
    assert_eq!(cap.breadcrumb.as_deref(), Some("Buf > cap"));
    assert_eq!(named(&tight, "constructor").kind, ChunkKind::Method);
    assert_eq!(named(&tight, "drop").kind, ChunkKind::Method);
}

#[test]
fn every_truncation_of_every_language_keeps_sane_bounds() {
    // A file can be read while it is being written, so every prefix of a
    // source is a real input: an unterminated string, a heredoc with no label
    // back, a regex that never closes. None of them may panic, and none may
    // produce a chunk whose lines point outside the file.
    let sources: &[(&str, &str)] = &[
        ("a.rs", include_str!("../src/scanner/mod.rs")),
        (
            "a.js",
            "const re = /a{2}/g;\nclass C {\n  m() { return `x ${ {a:1} }`; }\n}\n",
        ),
        (
            "a.py",
            "T = \"\"\"\ndef f():\n\"\"\"\nclass C:\n    def m(self):\n        return {1: 2}\n",
        ),
        (
            "a.php",
            "<?php\n$a = <<<EOT\n{ }\nEOT;\nclass C { function m() {} }\n",
        ),
        ("a.json", "{\"a\": [1, {\"b\": \"}\"}], \"c\": 2}\n"),
        (
            "a.md",
            "# A\n\ntext\n\n```rust\n# no\n```\n\nTitle\n=====\n",
        ),
        ("Client.java", JAVA_SRC),
        ("Client.kt", KOTLIN_SRC),
        ("client_test.go", GO_SRC),
        ("buf.axle", AXLE_SRC),
    ];
    for (path, full) in sources {
        let lines = full.lines().count() as u32 + 1;
        for cut in (0..full.len()).filter(|i| full.is_char_boundary(*i)) {
            // A tight budget too, so the truncated containers are recursed into
            // and not only kept whole.
            let mut out = chunks(path, &full[..cut]);
            out.extend(chunks_under(path, &full[..cut], 120));
            for c in &out {
                assert!(c.start_line >= 1, "{path}@{cut}: {c:?}");
                assert!(c.end_line >= c.start_line, "{path}@{cut}: {c:?}");
                assert!(c.end_line <= lines, "{path}@{cut}: {c:?}");
                assert!(!c.text.trim().is_empty(), "{path}@{cut}: {c:?}");
            }
        }
    }
}

#[test]
fn every_language_keeps_a_brace_inside_a_literal_out_of_its_counting() {
    // One file per language, each holding a brace or a comma that only a
    // scanner can tell from structure. A language that reads one structurally
    // fuses two items into one and loses the second one's name.
    let cases: &[(&str, &str, usize, &[&str])] = &[
        (
            "src/a.rs",
            "fn a() -> char { '{' }\n\nfn b() {}\n",
            2400,
            &["a", "b"],
        ),
        (
            "src/a.js",
            "const re = /a{2}/;\n\nfunction keep() {}\n",
            2400,
            &["re", "keep"],
        ),
        (
            "src/a.php",
            "<?php\n$a = <<<EOT\n{ } }\nEOT;\nfunction keep() {}\n",
            2400,
            &["keep"],
        ),
        (
            "app/a.py",
            "T = \"\"\"{ def nope(): \"\"\"\n\ndef keep():\n    pass\n",
            2400,
            &["T", "keep"],
        ),
        (
            "a.json",
            "{\n  \"a\": \"}{ ,\",\n  \"b\": 1\n}\n",
            20,
            &["a", "b"],
        ),
        (
            "A.java",
            "class A { String t = \"\"\"\n} \\\"\"\" }\n\"\"\"; }\n\nclass Keep {}\n",
            2400,
            &["A", "Keep"],
        ),
        (
            "a.kt",
            "fun a() = \"${m[\"}\"]} }\"\n\nfun keep() {}\n",
            2400,
            &["a", "keep"],
        ),
        (
            // A template that never closes costs its own line, not the file.
            "b.kt",
            "fun a() = \"${\"\nfun b() = 1\nfun c() = 2\n",
            2400,
            &["a", "b", "c"],
        ),
        (
            // A half-typed template is text of its string: the `)` after it
            // still closes the call, and nothing below is swallowed.
            "c.kt",
            "fun a() {\n    println(\"${x\")\n}\n\nfun b() {}\n\nfun c() {}\n",
            2400,
            &["a", "b", "c"],
        ),
        (
            "a.go",
            "func a() string { return `}` + string('}') }\n\nfunc keep() {}\n",
            2400,
            &["a", "keep"],
        ),
    ];
    for (path, src, budget, expected) in cases {
        let out = chunks_under(path, src, *budget);
        let got = names(&out);
        for symbol in *expected {
            assert!(
                got.iter().any(|n| n == symbol),
                "{path}: expected `{symbol}` among {got:?}"
            );
        }
    }
}
