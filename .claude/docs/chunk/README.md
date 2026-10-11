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
comment, or a raw literal for a structural brace**. That's what
`scanner/` guarantees, for every brace-delimited language, from one `Lexicon`
per language (`scanner/lexicon.rs`):

| | Rust | JavaScript / TypeScript | PHP | JSON |
|---|---|---|---|---|
| line comments | `//` | `//` | `//`, `#` | `//` (jsonc) |
| block comment | `/* */`, **nested** | `/* */` | `/* */` | `/* */` |
| strings | `"…"` | `"…"`, `'…'`, `` `…` `` | `"…"`, `'…'` | `"…"` |
| char literals vs lifetimes | yes | no | no | no |
| raw / byte literals | `r#"…"#`, `b"…"` | no | no | no |
| regex literals | no | `/…/flags` | no | no |
| heredocs | no | no | `<<<EOT … EOT` | no |
| `#[…]` attributes | yes | no | yes | no |
| `@Decorator(…)` | no | yes | no | no |

The regex heuristic is the classic one: `/` opens a literal unless the token
before it is an operand — an identifier, a number, `)`, `]`, `}` or a closing
quote — so `a / b` stays a division and `return /a{2}/` is a literal.

Matching tests: `braces_inside_strings_do_not_close_an_item`,
`raw_strings_and_nested_block_comments_are_opaque`,
`lifetimes_are_not_char_literals`,
`a_character_literal_costs_its_own_width`,
`a_regex_literal_is_opaque_only_where_an_expression_can_start`,
`a_braced_heredoc_does_not_close_the_function`.

### Where an item ends

A top-level item ends at the closing brace of its body, or at the `;` that
terminates a body-less one — with three exceptions the scanner alone cannot
see, and which the splitter handles:

- a brace that closes **inside** an open `(` or `[` does not end the item:
  `static X: Lazy<u8> = Lazy::new(|| { … });` is one item;
- a brace followed by `>` or `,` is a const-generic default, not a body:
  `struct A<const N: usize = { 3 }>;` is one item;
- a brace followed by `from`, `=`, `else`, `catch`… does not end a
  JavaScript statement: `import { readFile } from 'node:fs';` is one item.

If a real AST ever becomes necessary (type resolution, a call graph), the
`Chunker` trait is already the seam: an implementation gets added, nothing
else moves.

## The brace-delimited chunkers — Rust, JavaScript, PHP, JSON

`brace.rs` holds the pipeline they share: split, classify, recurse into an
oversized container, window what is still too big. A language is a `Profile`:
its lexicon, its keyword table, its preamble.

1. The file-level preamble becomes its own chunk when it carries words — the
   Rust `//!` / `#![…]` block becomes a `Module` chunk. A JavaScript shebang
   and a PHP `<?php` are dropped: neither says anything.
2. Top-level items: an item starts at its **first non-blank byte**, which
   naturally absorbs its `///` and its `#[…]`. `#` comments in PHP and
   `@Decorator(…)` in TypeScript ride along the same way — the decorator's
   argument list is delimited by `(`/`[`/`{`, tracked so that a `{` inside it
   is not mistaken for the class body that follows. An attribute and the
   declaration that SHARES its line are split apart: `#[inline] fn f() {}` is
   one item named `f`, not a line of trivia.
3. An item bigger than `max_chars` whose kind is a container (`impl`,
   `trait`, `mod`, `struct`/`class`) is **recursed**: one chunk per member,
   with a breadcrumb `Widget > render`. The recursion is real, so
   `mod a > Widget > render` works.
4. A leaf that's still too big is windowed on line boundaries, with overlap.

### What a chunk carries

| Field | Example | Role |
|-------|---------|------|
| `kind` | `method` | filter and ranking signal |
| `symbol` | `render` | short name |
| `breadcrumb` | `Widget > render` | locating it out of context |
| `start_line`/`end_line` | `12`/`48` | the `path:12-48` the agent cites |

| Language | `Function` | `Struct` | `Const` | `Method` |
|---|---|---|---|---|
| Rust | `fn` | `struct`, `union` | `const`, `static`, `type` | `fn` in `impl`/`trait` |
| JavaScript | `function` | `class` | `const`, `let`, `var` | member of a class |
| PHP | `function` | `class` | `const` | `function` in a class |
| Python | `def`, `async def` | `class` | `NAME = …` and `NAME: T = …` at top level | `def` in a class |

An item under `#[cfg(test)]` / `#[test]` becomes `kind = test`, and
`include_tests = false` excludes it entirely. `export`, `public`, `static`,
`async`, `abstract`, `final`, `readonly` and `override` are modifiers: the scan
skips them and the keyword still names the item.

`.jsx` and `.tsx` are deliberately **not** routed to the JavaScript chunker:
JSX text may hold an apostrophe (`<p>don't</p>`), which a lexical scanner reads
as the opening of a string and lets run across the markup. They take the text
fallback, where nothing is claimed about their structure.

## The Python chunker

Python has no braces: a block is what is indented further than its header.
`langs/python.rs` splits on **statements at column zero**, using a line pass that
knows where a bracket is still open, where a triple-quoted string is still
open and where a backslash continues the line — so a `def` inside a docstring
is not a definition, and `SETTINGS = { … }` over four lines stays one item.

An item takes its leading comments and decorators with it. A big `class` is
recursed into its methods; a module docstring becomes the `Module` chunk.

## The Markdown chunker

Splits on ATX headings, with **full ancestry** as breadcrumb
(`# Guide > ## Install > ### Windows`), so a fragment stays locatable out of
context. Setext headings (`Title` over `=====`) are recognised too, and claim
the line above them.

Three kinds of block are opaque, and each one is a real failure avoided:

- fenced blocks (` ``` ` or `~~~`). A closing fence carries **nothing but
  whitespace**, so ```` ```rust ```` inside a block is content, not a closure;
- indented blocks of four columns, where `#` is a shell comment and not a
  heading;
- front matter — `---` or `+++` on the first line, **and a closing line of the
  same**, so `title:` is not a Setext title. A document that opens on a `---`
  with no second one opens on a thematic break, and its headings survive.

A section that's still too big goes through the same windows as everything
else.

## The fallback

`Lang::Toml` and `Lang::Other` go through `TextChunker`: the whole file if it
fits, otherwise line windows.

Adding a language is a `Profile` in this crate — its `Lexicon`, its keyword
table, optionally a preamble — plus one line in `Registry`, one arm in
`Lang::from_path` / `as_str` / `parse`, and one entry in `DEFAULT_EXTENSIONS`
(a language nobody walks is a language nobody indexes). For a brace-delimited
language that is a few dozen lines, since `brace.rs` holds the pipeline; a
language whose blocks are not braces needs its own splitter, as
`langs/python.rs` is.

## Merging tiny fragments

Runs of undersized **anonymous** fragments are merged — a run of `use`,
`import` or `from … import` lines. A chunk that **names** something is never
merged away.

That is a rule about the keyword leg, not about size. Folding a one-line
`const MAX_RETRIES` into the `use` block above it would demote it to plain
text: its `kind` disappears from every filter, and its symbol leaves the FTS
column that BM25 weights four times — which is precisely the query
(`MAX_RETRIES`, `with_backoff`) the hybrid search claims to win. Locked by
`a_named_chunk_is_never_folded_into_its_neighbour` and by the registry-level
`every_symbol_a_chunker_produces_survives_the_merge`.

## Settings

| Key | Default | Effect |
|-----|---------|--------|
| `max_chars` | 2400 | hard ceiling, in **bytes** |
| `min_chars` | 120 | anonymous-neighbour merge threshold, in bytes |
| `overlap_chars` | 200 | window overlap, in bytes |
| `include_tests` | true | index test items |

The keys keep their historical names, but the unit is `str::len`, not
`chars`: a French, Cyrillic or CJK comment reaches the ceiling sooner than its
character count suggests, since those characters cost two to four bytes each.
Renaming the keys would invalidate every configuration file already written,
so the unit is documented instead of spelled.

Raising `max_chars` gives fewer, larger excerpts: better context, less
precision, more tokens per hit. 2400 is the compromise chosen for Rust; a
documentation repo can take more.
