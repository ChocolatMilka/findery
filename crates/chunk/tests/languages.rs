//! End-to-end chunking of a realistic file per language, through the registry.
//!
//! INTENT: the per-module tests pin down one rule each. This one answers the
//! question a user actually asks — "what does my project look like once it is
//! indexed?" — and it is where a language proves it produces named items
//! rather than one anonymous blob.

use findery_chunk::Registry;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};

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
    let src = "\
import { readFileSync } from 'node:fs';

const DEFAULTS = { retries: 3 };

// Retry with exponential backoff.
async function withBackoff(fn, attempts = DEFAULTS.retries) {
  for (let i = 0; i < attempts; i++) {
    try {
      return await fn();
    } catch (e) {
      if (i === attempts - 1) throw e;
    }
  }
}

export class Client {
  constructor(base) {
    this.base = base;
  }

  async get(path) {
    return fetch(`${this.base}${path}`);
  }
}

export default Client;
";
    let out = chunks("src/client.js", src);
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
    let src = "\
export interface RetryOptions {
  attempts: number;
}

export type Handler = (code: number) => string;

export const MAX_ATTEMPTS = 5;

export function retry<T>(fn: () => T, options: RetryOptions): T {
  return fn();
}
";
    let out = chunks("src/retry.ts", src);
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
    let src = "\
\"\"\"Rate limiting.\"\"\"

import time

DEFAULT_WINDOW = 60


def sleep_backoff(attempt):
    \"\"\"Sleep for the attempt's window.\"\"\"
    time.sleep(2 ** attempt)


class Limiter:
    \"\"\"A token bucket.\"\"\"

    def __init__(self, limit):
        self.limit = limit

    def take(self):
        return self.limit > 0
";
    let out = chunks("app/limits.py", src);
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
    let src = "\
<?php

declare(strict_types=1);

namespace App\\Http;

use App\\Support\\Signer;

class Client
{
    public const TIMEOUT = 30;

    public function __construct(private Signer $signer)
    {
    }

    public function send(string $body): string
    {
        return $this->signer->sign($body);
    }
}
";
    // A class that fits the budget stays whole, methods included.
    let out = chunks("src/Http/Client.php", src);
    assert_eq!(names(&out), vec!["App\\Http", "Client"], "got {out:#?}");
    let client = named(&out, "Client");
    assert_eq!(client.kind, ChunkKind::Struct);
    assert!(client.text.contains("TIMEOUT"));
    assert!(client.text.contains("function send"));

    // Under pressure it splits per member, each with its breadcrumb.
    let tight = chunks_under("src/Http/Client.php", src, 200);
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
    let src = "\
//! Backoff.

use std::time::Duration;

/// How many times to retry.
pub const MAX_ATTEMPTS: u32 = 5;

pub fn with_backoff(attempt: u32) -> Duration {
    Duration::from_millis(1 << attempt)
}

pub struct Policy {
    pub attempts: u32,
}
";
    let out = chunks("src/backoff.rs", src);
    assert_eq!(
        names(&out),
        vec!["backoff", "MAX_ATTEMPTS", "with_backoff", "Policy"],
        "got {out:#?}"
    );
    assert_eq!(out[0].kind, ChunkKind::Module);
    assert_eq!(named(&out, "MAX_ATTEMPTS").kind, ChunkKind::Const);
    assert_eq!(named(&out, "Policy").kind, ChunkKind::Struct);
}

#[test]
fn every_truncation_of_every_language_keeps_sane_bounds() {
    // A file can be read while it is being written, so every prefix of a
    // source is a real input: an unterminated string, a heredoc with no label
    // back, a regex that never closes. None of them may panic, and none may
    // produce a chunk whose lines point outside the file.
    let sources: &[(&str, &str)] = &[
        ("a.rs", include_str!("../src/scanner.rs")),
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
    ];
    for (path, full) in sources {
        let lines = full.lines().count() as u32 + 1;
        for cut in (0..full.len()).filter(|i| full.is_char_boundary(*i)) {
            let out = chunks(path, &full[..cut]);
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
