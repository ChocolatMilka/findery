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

| | Rust | JavaScript / TypeScript | PHP | JSON | Axle | Java | Kotlin | Go |
|---|---|---|---|---|---|---|---|---|
| line comments | `//` | `//` | `//`, `#` | `//` (jsonc) | `//` | `//` | `//` | `//` |
| block comment | `/* */`, **nested** | `/* */` | `/* */` | `/* */` | `/* */` | `/* */` | `/* */`, **nested** | `/* */` |
| strings | `"…"` | `"…"`, `'…'`, `` `…` `` | `"…"`, `'…'` | `"…"` | `"…"`, `` `…` `` | `"…"`, one line | `"…"`, one line; `` `ident` `` | `"…"`, one line |
| multi-line literals | `r#"…"#`, `b"…"` | no | `<<<EOT … EOT` | no | no | `"""…"""` (escaped) | `"""…"""` (raw) | `` `…` `` (verbatim) |
| `${…}` templates followed into | no | no (read flat) | no | no | no (read flat) | no | **yes** | no |
| char literals | yes, vs lifetimes | no | no | no | yes | yes | yes | yes (runes) |
| regex literals | no | `/…/flags` | no | no | no | no | no | no |
| `#[…]` attributes | yes | no | yes | no | no | no | no | no |
| `@Decorator(…)` | no | yes | no | no | yes (`@link(…)`, `@inline(never)`) | yes (`@Override`) | yes (`@Test`, `@file:…`) | no |
| statement ends at | `;` | `;` | `;` | `;` | `;` | `;` | `;` **or newline** | `;` **or newline** |

The regex heuristic is the classic one: `/` opens a literal unless the token
before it is an operand — an identifier, a number, `)`, `]`, `}` or a closing
quote — so `a / b` stays a division and `return /a{2}/` is a literal.

A string that cannot span a line (Java, Kotlin, Go) ends at the newline when
its quote never comes: an unterminated `"open {` costs its own line, not every
item below it.

A Kotlin template is code — `"${m["}"]}"` holds a string that holds a brace —
so the scanner re-enters it to find its matching `}`, at most 16 templates deep
(a file of nested `"${` costs a misread string, never the stack).

A template runs over a line only inside a bracket it opened — `"${minOf(` /
`size,` / `)}"` — and over 16 lines at most; anywhere else a newline before its
`}` means it never closed.

A template that never closed is read as plain text of its literal, which is
read again once, flat: a half-typed `println("${x")` ends at its quote, its `)`
still closes the call, and nothing below it is swallowed. Each literal is read
at most twice — where letting every enclosing template rescan the rest of the
file made thirty `"${` lines take longer than a sync (`scanner/templates.rs`).

A Kotlin backticked identifier is opaque on its line, so the apostrophe of
`` fun `it's sent`() `` opens nothing.

Matching tests:

- `braces_inside_strings_do_not_close_an_item`
- `raw_strings_and_nested_block_comments_are_opaque`
- `lifetimes_are_not_char_literals`
- `a_character_literal_costs_its_own_width`
- `a_regex_literal_is_opaque_only_where_an_expression_can_start`
- `a_braced_heredoc_does_not_close_the_function`
- `a_text_block_is_one_literal_and_an_escape_does_not_close_it`
- `a_template_holding_a_string_holding_a_brace_stays_inside_its_string`
- `a_template_nested_past_the_cap_still_terminates`
- `a_template_that_does_not_close_on_its_line_is_text_of_its_string`
- `a_template_runs_over_lines_inside_a_bracket_it_opened`
- `unclosed_templates_cost_linear_time`
- `every_language_keeps_a_brace_inside_a_literal_out_of_its_counting`
- `an_unterminated_one_line_string_stops_at_the_newline`
- `a_go_raw_string_spans_lines_and_knows_no_escape`

### Where an item ends

A top-level item ends at the closing brace of its body, or at the `;` that
terminates a body-less one.

**Go and Kotlin** insert their semicolons in the lexer, so their `Lexicon`
says `Statements::Newline`: there a newline at depth zero ends the item, when
the item already holds code and its last token can end a statement:

- an identifier, a literal, `)`, `]`, `}`, `++`/`--`;
- and for Kotlin only a type's `>`, `?`, `!!` or an `import a.*` (in Go those
  are operators, so `var b = x >` runs on to its next line).

A `}` closes nothing on its own in these two languages: it may close a type
literal — `func Get() interface{} {`, `<-chan struct{} {`, `struct{}{A: 1}` —
and the newline after it ends the statement anyway, `} // end` included.

The rule follows Go's semicolon insertion for every token it knows; a line
ending on an operator (`=`, `+`, `,`, `:`) runs on.

A Kotlin line followed by one opening on `.`, `?`, `:`, `=`, `{`, `&&`, `||`,
`where`, `get`, `set`, `else`, `catch` or `finally` runs on too (a call chain,
a supertype list, a property accessor, the other branch of an `if` or a
`try`).

Annotations, comments and modifiers in front of such a line do not hide it:
`private set` and `inline get() = …` continue their property — the modifiers
are `private`, `protected`, `internal`, `public`, `actual`, `expect`, `inline`
and `external`.

A line ending on one of them, or on `else`, cannot end a statement:
`class A private` over `constructor(…)` is one class (a word after a `.` is a
name, so `package okio.internal` ends where it stands).

`constructor` continues only a **class header** with no body yet, and only
behind an annotation or a modifier:

- `class Foo` over `@Inject constructor(…)`, over
  `internal actual constructor(…)`, over a KDoc and `@Inject` /
  `constructor(…)` on two lines, is one class;
- while a secondary `internal constructor(…)` below a property or a `}` is a
  member of its own.

An annotation never ends a statement: it annotates what follows it (`this@A`
and `return@forEach` are labels, not annotations). A comment or an annotation
alone on its line is trivia waiting for its item: `// Run runs.` and `@Test`
stay with the `func`/`fun` below them.

Locked by:

- `a_go_newline_ends_a_statement_where_the_go_lexer_would`
- `a_newline_inside_brackets_or_after_an_operator_does_not_end_a_statement`
- `a_brace_ends_no_newline_statement_before_its_line_does`
- `a_brace_closing_a_type_literal_does_not_end_the_function`
- `an_annotation_or_a_comment_alone_on_its_line_waits_for_its_item`
- `a_kotlin_line_opening_on_a_continuation_continues_the_statement`
- `a_kotlin_annotated_constructor_under_its_class_continues_the_class`
- `a_primary_constructor_on_the_next_line_stays_with_its_class`
- `a_secondary_constructor_is_a_member_of_its_own`
- `a_modifier_word_inside_a_qualified_name_ends_nothing`
- `a_kotlin_accessor_behind_a_modifier_continues_its_property`
- `an_inline_or_external_accessor_stays_with_its_property`
- `a_catch_or_an_else_on_the_next_line_continues_the_statement`
- `a_kotlin_else_on_its_own_line_continues_the_if`

Three exceptions to the brace rule the scanner alone cannot see are handled by
the splitter, in every `;` language:

- a brace that closes **inside** an open `(` or `[` does not end the item:
  `static X: Lazy<u8> = Lazy::new(|| { … });` is one item;
- a brace followed by `>` or `,` is a const-generic default, not a body:
  `struct A<const N: usize = { 3 }>;` is one item;
- a brace followed by `from`, `=`, `else`, `catch`… does not end a
  JavaScript statement: `import { readFile } from 'node:fs';` is one item.

The **body** of an item — what a container is recursed into — opens on the
first `{` past its trivia that sits outside every `(…)`, `[…]` and `<…>`.

The braces of `@SuppressWarnings({"a"})`, of a decorator's `({ selector })`,
of a default lambda `onClick: () -> Unit = { … }` or of `impl Foo<{ N }>` are
not a body; taking one for it once dropped the real body, members and all.

Only a `<` glued to a name (`Vec<`, `impl<`, `::<`) opens type arguments: the
`<` of `ok: Boolean = x < 1` or of `[u8; 1 << 4]` compares, and counting it hid
the body of the class it sat in.

Locked by:

- `a_brace_inside_an_annotation_or_a_parameter_list_is_not_the_body`
- `a_comparison_in_a_parameter_list_does_not_hide_the_body`

If a real AST ever becomes necessary (type resolution, a call graph), the
`Chunker` trait is already the seam: an implementation gets added, nothing
else moves.

## The brace-delimited chunkers — Rust, JavaScript, PHP, Axle, Java, Kotlin, Go

`brace.rs` holds the pipeline they share: split, classify, recurse into an
oversized container, window what is still too big. A language is a `Profile`:
its lexicon, its keyword table, its preamble.

`header.rs` holds what every profile reads an item with:

- **where its trivia stops** — `split_trivia`;
- **which annotations it carries** — `annotation_names`, `strip_annotations`;
- **what names it** — `keyword_header`, `next_ident`;
- **the one bracket matcher of the crate** — `balanced_end`.

An annotation's name is spelled once (`annotation_name_end`), use-site target
included: `@file:JvmName(…)` and `@field:Json(…)` are one annotation each, for
every reader. A `@word` the lexicon lists in `at_keywords` — Java's
`@interface` — is code, not an annotation.

A header is read up to its first `{`, `;` or `=` **outside** any `(…)`/`[…]`
and any string (`header_of`, every profile but JavaScript's and PHP's), so a
parameter default (`x: Int = 3`) or an annotation argument (`@Size(min = 1)`)
never cuts the name off.

Java and Kotlin read an item the same way (`annotated_header`): its test flag,
its header with its annotations stripped, and what follows it.

1. The file-level preamble becomes its own chunk when it carries words —
   the Rust `//!` / `#![…]` block becomes a `Module` chunk, windowed like any
   other chunk when it outgrows `max_chars`. A JavaScript shebang and a PHP
   `<?php` are dropped: neither says anything.
2. Top-level items: an item starts at its **first non-blank byte**, which
   naturally absorbs its `///` and its `#[…]`.

   `#` comments in PHP and `@Decorator(…)` in TypeScript ride along the same
   way — the decorator's argument list is delimited by `(`/`[`/`{`, tracked so
   that a `{` inside it is not mistaken for the class body that follows.

   An attribute and the declaration that SHARES its line are split apart:
   `#[inline] fn f() {}` is one item named `f`, not a line of trivia.
3. An item bigger than `max_chars` whose kind is a container (`impl`,
   `trait`, `mod`, `struct`/`class`) is **recursed**: one chunk per member,
   with a breadcrumb `Widget > render`.

   Its **header** — doc comment, annotations, supertypes, up to the `{` — is a
   chunk of its own, with the container's kind and symbol: no member carries
   it, and dropping it lost the class's javadoc and the class itself as a
   search hit.

   The recursion is real, so `mod a > Widget > render` works — but only
   `MAX_NESTING` (16, in `sizing.rs`) containers deep, Python's classes
   included: past that, what is left is windowed as one leaf. A recursion
   whose depth the input controls is a stack the input controls, and chunking
   runs inline in the daemon's sync, where an overflow aborts the process; ten
   thousand nested classes cost sixteen passes over the file and about 30 ms
   (it was a crash at six thousand).

   A profile may also name a member kind that is split **whenever its parent
   is**, however small (`splits_with_parent`): Axle's `when T : Bound { … }`
   groups, whose methods would otherwise hide inside one chunk and lose their
   `method` kind and their symbols; and a declaration group split at any size
   (`group_body`): Go's `type ( … )`.

   The header of an undocumented test module — `#[cfg(test)]` over
   `mod tests {` — is folded together with the anonymous `use super::*;` below
   it rather than left as a 25-byte chunk that answers nothing; a member that
   names something is never folded.
4. A leaf that's still too big is windowed on line boundaries, with
   overlap (`sizing::split_windows`).

   A line longer than `max_chars` — a minified bundle, a generated table — is
   cut anyway, after its last break (anything but a letter, a digit or `_`)
   that fits, or at a character boundary when no break does: a word is only
   ever cut when it is longer than the whole budget.

### No content is dropped, no chunk outgrows its budget

Every word of every source line is carried, as a whole word, by a chunk whose
line range covers that line. The only exceptions:

- the JavaScript shebang and the PHP `<?php` tag, dropped on purpose (step 1);
- a word longer than `max_chars`, which no chunk can hold.

No chunk is longer than `max_chars`, except by less than one character when the
budget is narrower than a single UTF-8 character (3 bytes at most).

A symbol is carried by the text of the chunks that bear it: a name pointing at
chunks that do not hold its declaration is a split in the wrong place.

A wrong split fails loudly, on a name or a kind; a dropped body fails in
silence, so all of this is checked mechanically. `tests/coverage.rs` runs it,
at `max_chars` 120 and 2400, over:

- every language's fixture (`tests/fixtures/mod.rs`);
- the crate's own sources;
- every shape that once misled a chunker;
- the real-world shapes a corpus of 2,550 files found (with the line each
  declaration must still be named on);
- and twenty thousand nested containers per language, on a 256 KiB stack, in
  bounded time.

A chunker is pure, and `chunking_is_deterministic` checks the part of it a test
can see: every language gives the same chunks for the same text, from a fresh
registry and after another file — what an output depending on a hashed
collection's order, or on state carried between files, would break.

### Line endings

`Registry::chunk` reads every `\r\n` as `\n`, before any chunker sees the
text — the one place it is done.

A Windows checkout (`core.autocrlf`) and a Linux one of the same commit then
give the same chunks: the same boundaries (`max_chars` used to count the
`\r`s) and the same text, hence the same content hashes, where 1,104 of 1,334
file × budget pairs of a real corpus differed.

Line numbers do not move: only the `\r` before a `\n` goes. A lone `\r` is no
line ending to `str::lines`, git or the line index, and stays content. Locked
by `a_crlf_checkout_chunks_exactly_like_an_lf_one`.

This changes the chunk text of a CRLF file once: each one already indexed
re-embeds its chunks the next time it is chunked again — when it changes, or
on a `findery sync --full` — and never again after that.

The same holds for every other change of this page's rules: gate 2 does not
re-chunk an unchanged file, so an upgrade costs nothing until a file is
touched.

### What a chunk carries

| Field                   | Example           | Role |
|-------------------------|-------------------|------|
| `kind`                  | `method`          | filter and ranking signal |
| `symbol`                | `render`          | short name |
| `breadcrumb`            | `Widget > render` | locating it out of context |
| `start_line`/`end_line` | `12`/`48`         | the `path:12-48` the agent cites |

| Language | `Function` | `Struct` | `Const` | `Method` |
|---|---|---|---|---|
| Rust | `fn` | `struct`, `union` | `const`, `static`, `type` | `fn` in `impl`/`trait` |
| JavaScript | `function` | `class` | `const`, `let`, `var` | member of a class |
| PHP | `function` | `class` | `const` | `function` in a class |
| Axle | `fn`, `extern "C" fn` | `class`, `struct`, `exception` | `const`, `type`, `static` field | `fn`, `static fn`, `constructor`, `drop` in a container |
| Java | — | `class`, `record` | `static final` field | method or constructor in a type, a record's compact constructor |
| Kotlin | `fun` at top level (`fun T.ext` → `T.ext`) | `class`, `data`/`sealed`/`value class`, `object`, `companion object` | `val`/`var` at top level, `const val`, `typealias` | `fun`, `constructor` in a class |
| Go | `func name` | `type X struct` | `const`, `var` (groups too), `type X = Y`, `type X Y` | `func (r *Recv) name` → `Recv.name`; a method of an `interface` |
| Python | `def`, `async def` | `class` | `NAME = …` and `NAME: T = …`, at top level and in a class body | `def` in a class |

A module chunk — the Rust `//!` preamble, the Python module docstring — is
named after its file, `src/path_map.rs` → `path_map`.

A file that stands for its directory takes the directory's name, a crate's
`src` skipped: `src/langs/mod.rs` → `langs`, `crates/core/src/lib.rs` →
`core`, `src/bin/tool/main.rs` → `tool`, `app/__init__.py` → `app`.
Thirty-three files all named `mod` said nothing. In a registry checkout the
crate directory carries its version (`anyhow-1.0.102`), and so does the name.

The two remaining kinds:

- **`Trait`** — Rust `trait`, PHP `interface`/`trait`, TypeScript
  `interface`, Axle `trait`, Java `interface` and `@interface`, Kotlin
  `interface` and `fun interface`, Go `type X interface`.
- **`Enum`** — Rust/PHP/TS/Axle `enum`, Java `enum`, Kotlin `enum class`. A
  Java or Kotlin `enum` is recursed like a class.

An item under `#[cfg(test)]` / `#[test]` becomes `kind = test`, and
`include_tests = false` excludes it entirely.

Java and Kotlin read the same from an annotation of an exact set
(`header::is_test_annotation`):

- the method markers `@Test`, `@ParameterizedTest`, `@RepeatedTest`,
  `@TestFactory`, `@TestTemplate`, `@Theory`, `@Property`, whatever their
  package (`@org.junit.jupiter.api.Test`);
- and the framework test slices that make a whole class one
  (`@SpringBootTest`, `@WebMvcTest`, `@DataJpaTest`, `@QuarkusTest`,
  `@HiltAndroidTest`…).

A suffix is not enough: TestNG's `@BeforeTest` sets a test up and okio's
`@InterceptTest` decorates a field, and `include_tests = false` keeps both. A
javadoc `@see Test` is a comment, not an annotation.

Go has no attribute: in a `_test.go` file (`main_test.GO` too, as `Lang` routes
it), a function named `Test`, `Benchmark`, `Fuzz` or `Example` followed by
anything but a lowercase letter is a test (`TestParse` is, `Testify` is not),
which is why the Go profile is built per file from its path.

What `include_tests = false` drops therefore differs by language, on purpose:
it drops what the language itself marks as a test.

- **Rust** — a `mod tests` goes whole, helpers included, since its attribute
  covers them.
- **Java, Kotlin** — a test class goes whole only when a test slice marks the
  class; otherwise each marked method goes and its unmarked helpers stay.
- **Go** — a `_test.go` file keeps its helpers, since only the `TestXxx`
  naming marks a test.

`export`, `public`, `static`, `async`, `abstract`, `final`, `readonly` and
`override` are modifiers: the scan skips them and the keyword still names the
item.

`.jsx` and `.tsx` are deliberately **not** routed to the JavaScript chunker:
JSX text may hold an apostrophe (`<p>don't</p>`), which a lexical scanner reads
as the opening of a string and lets run across the markup. They take the text
fallback, where nothing is claimed about their structure.

### Axle

`.axle` runs the shared pipeline over `Lexicon::AXLE`; the syntax is still
moving, so the whole language lives in one keyword table in `langs/axle.rs`.

`pub`, `pub(crate)`, `pub(file)`, `pub(derived)`, `async`, `static`,
`readonly`, `own`, `shared` and `extern "C"` are modifiers the scan walks
past, so `pub static fn isAscii` is a `fn` named `isAscii`.

`trait` is a `Trait`, `enum` an `Enum`; `constructor` and `drop` are named
after the keyword (they have no identifier), so a hit on `drop` finds every
destructor.

A header ends at `{`, `;` or `=` outside strings and brackets (`header_of`), so
`const FN_NAME : string = "fn x"` is named `FN_NAME` and
`extern "C" from "lib{x}=1"` keeps its library name whole.

The real lexer reads `"…"` and `` `…` `` as one flat token (a template's `${…}`
is not re-entered), and the scanner does the same.

Three forms declare without a declaring keyword:

- `pub static MAX : i64 = 500;` — a class slot — is a `Const` named `MAX`; a
  `readonly id : i64;` field stays an anonymous field;
- `extern "C" from "user32" { … }` — a file of bindings — is one `Module`
  named `user32`, recursed per binding (`user32 > CreateWindowExW`) when it
  is too big; its `@target(os = windows)` rides with it;
- `when T : Printable { … }` inside a class is an `Impl` named
  `when T : Printable`, and it **splits whenever its class does**: its methods
  come out as `Wrapper > when T : Printable > tag`.

`@target(…)`, `@inline(…)`, `@heat(…)`, `@derive(…)`, `@layout(…)` and
`@link(…)` are annotations riding with the item below them.

### Java

`.java` runs the pipeline over `Lexicon::JAVA`.

A type is named by the word after `class`, `interface`, `@interface`, `enum` or
`record` — and `record` only when a component list follows, since a field may
be named `record`.

A member has no keyword: inside a type, a header with a parameter list is a
`method` named by the identifier before its `(` (`public <T> List<T> map(…)` →
`map`, a constructor → its type's name, `URL(String s) {}` included), and so is
a record's compact constructor, which has no list at all (`public Point { … }`
→ `Point`); `static { … }` and `{ … }` are initializers and stay anonymous.

Only inside an `enum` can that bare shape be a constant: an enum constant list
(`RED("r"), …;`, `Mercury(3.3e23), …;`) stays anonymous — told apart from a
constructor by having no body, or, for a constant with a body of its own
(`PLUS("+") { … }`, `IDENTITY() { … }`), by its SCREAMING name.

A constant with a body is anonymous by intent: its name is in its text, and its
body's methods are what a search wants. The profile knows which container a
member sits in (`Profile::classify` takes the parent's kind), which is what
tells the two apart.

A `static final` field is a `Const`. Annotations are stripped before the header
is read, so `public @Deprecated(since = "9") class Old` is `Old`. `package` and
`import` lines are anonymous and merge.

### Kotlin

`.kt` and `.kts` run the pipeline over `Lexicon::KOTLIN`, whose statements end
at the newline (above).

- `enum class` is an `Enum`, `fun interface` a `Trait`, an unnamed
  `companion object` is named `Companion` (Kotlin's own default).
- An extension keeps its receiver in the symbol: `fun <T> List<T>.second()` →
  `List.second`, `val String?.orBlank` → `String.orBlank`.
- A backticked name keeps its words, for a function as for a type:
  `` fun `sends once`() `` → `sends once`, `` class `My Class` `` →
  `My Class`, `` interface `-Iface` `` → `-Iface`.
- `fun<T> id(…)` is named `id`.
- A delegate is not part of the name: `val logger by lazy { … }` → `logger`.
- A `val`/`var` at the top level is a `Const`; inside a class it is a
  property, anonymous, unless it is a `const val`.
- `package` and `import` lines are anonymous and merge.

### Go

`.go` runs the pipeline over `Lexicon::GO`, whose statements end where the Go
lexer inserts its semicolons (above).

A method is declared at the top level with its receiver and is named after
both: `func (s *Server) Handle(…)`, `func (l *List[T]) Push(…)` and
`func (m *Map[K, V]) Get(…)` give `Server.Handle`, `List.Push` and `Map.Get`,
kind `method`.

`type X struct` is a `Struct`, `type X interface` a `Trait` (recursed into its
method set when big), any other `type` a `Const`; a name is read whole,
`type Größe struct` included; type parameters glued to the name
(`List[T any]`) are not mistaken for a slice type (`IDs []int`).

A `type ( … )` group declares its types one by one and is split at any size:
`type ( A struct{…}; B interface{…} )` gives a `Struct` named `A` and a `Trait`
named `B`, each breadcrumbed as if declared alone, plus the anonymous `type (`
head with its doc.

A `const (…)` or `var (…)` group is one `Const` named after its first member,
comments skipped — anonymous, it would be merged into its neighbours and lose
its kind.

The blank identifier names nothing: `var _ io.Reader = (*T)(nil)` stays
anonymous, and `const ( _ = iota; Fast … )` is named `Fast`. The `package`
clause, with its package doc, and the imports are anonymous and merge.

## The Python chunker

Python has no braces: a block is what is indented further than its header.

`langs/python.rs` splits on **statements at column zero**, using a line pass
that knows where a bracket is still open, where a triple-quoted string is still
open and where a backslash continues the line — so a `def` inside a docstring
is not a definition, and `SETTINGS = { … }` over four lines stays one item.

An item takes its leading comments and decorators with it, a decorator whose
argument list runs over several lines included (`@dataclass(` /
`frozen=True,` / `)`).

What comes before the first item of a block — a licence header, a file of
comments only, a lone `#!` line — is an item of its own: no line is dropped.

A big `class` is recursed into its methods, sixteen classes deep at most, and
its header — comments, decorators, the `class …:` statement over however many
lines it spans — is a chunk of its own, as in the brace pipeline.

An assignment in a class body is a class attribute, a `Const` like a top-level
one (`Widget > MAX`). A module docstring becomes the `Module` chunk.

Locked by:

- `a_file_of_comments_or_a_lone_shebang_still_yields_a_chunk`
- `the_lines_before_the_first_item_are_kept`
- `a_decorator_spanning_lines_stays_with_its_class`

## The JSON chunker

A JSON file is one document while it fits: `package.json` answered in one chunk
is worth more than its sections answered separately.

Too big, it becomes its top-level members, the KEY as symbol (`dependencies`,
`scripts`), recursed up to three levels.

A member that is split keeps its head — `"dependencies": {` — as a chunk of its
own, so the key a search names is never only in the breadcrumb of what it
holds; and its tail — what follows its closing bracket, a jsonc comment after
the last member — is a chunk too.

A key is read past the jsonc comments above it, and an empty key `""` names
nothing.

Locked by:

- `a_comment_after_the_closing_bracket_of_a_split_member_is_kept`
- `a_comment_above_a_member_does_not_hide_its_key`
- `an_empty_key_names_nothing`

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
table, optionally a preamble — plus:

- one line in `Registry`;
- one arm in `Lang::from_path` / `as_str` / `parse` and one entry in
  `Lang::ALL`;
- one entry in `DEFAULT_EXTENSIONS` (a language nobody walks is a language
  nobody indexes), mirrored in `config/findery.toml` and the
  `findery init --ext` help.

The two mirrors are checked against `DEFAULT_EXTENSIONS` by a test each
(`the_shipped_reference_config_parses_and_matches_the_defaults`,
`the_init_help_lists_every_default_extension`), so forgetting one fails
`cargo test`. The MCP `search` tool's `langs` enum is built from `Lang::ALL`
and has nothing to forget
(`the_search_tool_offers_every_language_and_parses_each_one`).

For a brace-delimited language that is a few dozen lines, since `brace.rs`
holds the pipeline; a language whose blocks are not braces needs its own
splitter, as `langs/python.rs` is.

## Merging tiny fragments

Runs of undersized **anonymous** fragments are merged — a run of `use`,
`import` or `from … import` lines. A chunk that **names** something is never
merged away.

That is a rule about the keyword leg, not about size. Folding a one-line
`const MAX_RETRIES` into the `use` block above it would demote it to plain
text: its `kind` disappears from every filter, and its symbol leaves the FTS
column that BM25 weights four times — which is precisely the query
(`MAX_RETRIES`, `with_backoff`) the hybrid search claims to win.

Locked by `a_named_chunk_is_never_folded_into_its_neighbour` and by the
registry-level `every_symbol_a_chunker_produces_survives_the_merge`. A merge
counts the newline it inserts, so a merged chunk still fits `max_chars`
(`a_merge_counts_the_newline_it_inserts`).

Before merging, a fragment that carries no word — blank, a lone `}` or `);` —
is dropped. A word is what the coverage invariant counts: letters, digits and
`_`, so `_ => {}` is kept (`a_fragment_whose_only_word_is_an_underscore_is_kept`).

## Settings

| Key             | Default | Effect |
|-----------------|---------|--------|
| `max_chars`     | 2400    | hard ceiling, in **bytes** — a one-line minified file included |
| `min_chars`     | 120     | anonymous-neighbour merge threshold, in bytes |
| `overlap_chars` | 200     | window overlap, in bytes |
| `include_tests` | true    | index test items |

The keys keep their historical names, but the unit is `str::len`, not
`chars`: a French, Cyrillic or CJK comment reaches the ceiling sooner than its
character count suggests, since those characters cost two to four bytes each.

Renaming the keys would invalidate every configuration file already written,
so the unit is documented instead of spelled.

Raising `max_chars` gives fewer, larger excerpts: better context, less
precision, more tokens per hit. 2400 is the compromise chosen for Rust; a
documentation repo can take more.
