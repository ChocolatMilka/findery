# `findery-chunk` — chunking without lying about boundaries

## The contract

A chunker is **pure**: same input, same output, no I/O, no clock, no
randomness (CONV-025). All of incremental sync rests on that — if
`chunk(text)` varied, gate 2 (content hash) would let unchanged files
through and the repo would be re-embedded on every pass.

The chunker knows **neither the project nor the hash**. It returns
`RawChunk`s; identity is stamped by L2, the single writer (CONV-024).

## Why not tree-sitter

Chunking needs **item boundaries**, not a syntax tree. A real parser
(tree-sitter, `syn`) would pull a C string or a proc-macro into the picture
for information we throw away immediately.

What we actually need is to **never mistake a brace inside a string, a
comment, or a raw literal for a structural brace**. That's exactly what
`scanner.rs` guarantees, in ~200 lines with no dependency:

- line and block comments, **nested** (Rust nests them)
- strings with escapes
- raw strings `r"…"`, `r#"…"#`, `br#"…"#`
- character literals **distinguished from lifetimes**: `'a'` is a literal,
  `'a` is a lifetime

Matching tests: `braces_inside_strings_do_not_close_an_item`,
`raw_strings_and_nested_block_comments_are_opaque`,
`lifetimes_are_not_char_literals`.

If a real AST ever becomes necessary (type resolution, a call graph), the
`Chunker` trait is already the seam: an implementation gets added, nothing
else moves.

## The Rust chunker

### Splitting

1. The `//!` / `#![…]` preamble becomes its own `Module` chunk — it
   documents the module, not whichever item happens to follow it.
2. Top-level items: an item starts at its **first non-blank byte**, which
   naturally absorbs its `///` and its `#[…]`. It ends at the closing brace
   of its body, or the `;` that terminates a bodyless item.
3. An `impl` / `trait` / `mod` bigger than `max_chars` gets **recursed**:
   one chunk per method, with a breadcrumb `Widget > render`.
4. A leaf that's still too big is windowed on line boundaries, with
   overlap.
5. Tiny neighbors (< `min_chars`) are merged: a three-line `impl Display`,
   embedded on its own, is noise.

### What a chunk carries

| Field | Example | Role |
|-------|---------|------|
| `kind` | `method` | filter and ranking signal |
| `symbol` | `render` | short name |
| `breadcrumb` | `Widget > render` | locating it out of context |
| `start_line`/`end_line` | `12`/`48` | the `path:12-48` the agent cites |

An item under `#[cfg(test)]` / `#[test]` becomes `kind = test`, and
`include_tests = false` excludes it entirely.

### The text actually embedded

```text
path: crates/index/src/sync.rs
lang: rust
symbol: Indexer > sync
---
<the body>
```

The path and the breadcrumb carry an enormous amount of signal in code
search: "where is the rate limit handled" is found in
`middleware/rate_limit.rs` as much by the path as by the body. They're
therefore systematically prefixed — and count toward the `content_hash`,
so moving a file does change its vector.

## The Markdown chunker

Splits on ATX headings, with **full ancestry** as breadcrumb
(`# Guide > ## Install > ### Windows`), so a fragment stays locatable out of
context.

Fenced code blocks (``` or ~~~) are opaque: a `# comment` inside a `sh`
block isn't a heading, and treating it as one would blow up every example
in the documentation.

## Settings

| Key | Default | Effect |
|-----|---------|--------|
| `max_chars` | 2400 | hard ceiling (~660 tokens on Rust) |
| `min_chars` | 120 | neighbor-merge threshold |
| `overlap_chars` | 200 | window overlap |
| `include_tests` | true | index test items |

Raising `max_chars` gives fewer, larger excerpts: better context, less
precision, more tokens per hit. 2400 is the compromise chosen for Rust; a
documentation repo can take more.

## The fallback

`Lang::Other` and `Lang::Toml` go through `TextChunker`: the whole file if
it fits, otherwise line windows. Adding a language = one `Chunker` + one
line in `Registry`, nothing else in the workspace.
