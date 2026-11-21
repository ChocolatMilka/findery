//! Go chunker: one chunk per top-level declaration, one per method of an
//! interface or field group of a struct big enough to be split.
//!
//! INTENT: Go rides the shared pipeline of [`crate::brace`] with a lexicon
//! that ends a statement at the newline where the Go lexer inserts its
//! semicolons, and reads `` `raw` `` strings and `'r'` runes as opaque. What
//! is Go's own is said here: a method is declared at the top level, with its
//! receiver, so `func (s *Server) Handle(…)` is found as `Server.Handle`; a
//! `type ( … )` group declares types one by one; and a test is a NAMING
//! convention of `_test.go` files — `TestXxx`, `BenchmarkXxx`, `FuzzXxx`,
//! `ExampleXxx` — not an attribute, which is why the profile knows the path
//! it chunks.

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

use crate::brace::{ItemMeta, Profile, chunk_with};
use crate::header::{PARENS, balanced_end, header_of, next_ident, opens_on, split_trivia};
use crate::items::Span;
use crate::scanner::Lexicon;

/// Splits Go source into items.
pub struct GoChunker;

impl Chunker for GoChunker {
    fn lang(&self) -> Lang {
        Lang::Go
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        let profile = GoProfile {
            // `Lang::from_path` routes `main_test.GO` here: so does this.
            test_file: path.to_ascii_lowercase().ends_with("_test.go"),
        };
        chunk_with(&profile, path, text, cfg)
    }
}

/// The prefixes `go test` runs a function for, in a `_test.go` file.
const TEST_PREFIXES: &[&str] = &["Test", "Benchmark", "Fuzz", "Example"];

/// Go as the shared pipeline sees it, for one file.
struct GoProfile {
    /// The file is a `_test.go` file, where a `TestXxx` function is a test.
    test_file: bool,
}

impl Profile for GoProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::GO
    }

    fn classify(&self, item: &str, parent: Option<ChunkKind>) -> ItemMeta {
        let lex = self.lexicon();
        let (_, code) = split_trivia(item, lex);
        let header = header_of(code, lex, b"{=;").trim_start();
        let keyword_len = header.bytes().take_while(u8::is_ascii_alphabetic).count();
        let (keyword, rest) = header.split_at(keyword_len);
        let (kind, symbol) = match keyword {
            "func" => func_header(rest),
            "type" => type_header(rest),
            "const" | "var" => (ChunkKind::Const, group_or_name(rest)),
            // A spec of a `type ( … )` group: `A struct{…}`, `ID = string`.
            _ if parent == Some(ChunkKind::Const) => type_header(header),
            // An interface's method set: `Read(p []byte) (n int, err error)`.
            _ if parent.is_some() => match member_name(header) {
                Some(name) => (ChunkKind::Method, Some(name)),
                None => (ChunkKind::Text, None),
            },
            _ => (ChunkKind::Text, None),
        };
        let is_test = self.test_file
            && kind == ChunkKind::Function
            && symbol.as_deref().is_some_and(is_test_name);
        ItemMeta {
            kind,
            symbol,
            is_test,
        }
    }

    /// INTENT: a `type ( … )` group declares several types — a struct, an
    /// interface — and as one chunk it was named after the first only, every
    /// other type's name and kind gone from the index.
    fn group_body(&self, src: &str, item: Span) -> Option<Span> {
        let lex = self.lexicon();
        let (trivia, code) = split_trivia(item.slice(src), lex);
        if !opens_on(code.as_bytes(), "type") {
            return None;
        }
        let after_keyword = &code["type".len()..];
        let rest = after_keyword.trim_start();
        if !rest.starts_with('(') {
            return None;
        }
        let open = item.start + trivia.len() + (code.len() - rest.len());
        let past_close = balanced_end(&src.as_bytes()[..item.end], open, lex, PARENS)?;
        Some(Span {
            start: open + 1,
            end: past_close - 1,
        })
    }
}

/// `Name(…)`, `Name[T any](…)` → `Function Name`; `(s *Server) Handle(…)` →
/// `Method Server.Handle`.
fn func_header(rest: &str) -> (ChunkKind, Option<String>) {
    let rest = rest.trim_start();
    let first_word = || next_ident(rest.split_whitespace().next());
    match rest.starts_with('(') {
        false => (ChunkKind::Function, first_word()),
        true => match balanced_end(rest.as_bytes(), 0, &Lexicon::GO, PARENS) {
            None => (ChunkKind::Function, None),
            Some(past_receiver) => (ChunkKind::Method, method_name(rest, past_receiver)),
        },
    }
}

/// `Server.Handle`, from `(s *Server) Handle(…)` whose receiver list ends
/// just before `past_receiver`.
fn method_name(rest: &str, past_receiver: usize) -> Option<String> {
    let receiver = receiver_type(&rest[1..past_receiver - 1]);
    let name = next_ident(rest[past_receiver..].split_whitespace().next());
    match (receiver, name) {
        (Some(r), Some(n)) => Some(format!("{r}.{n}")),
        (None, n) => n,
        (Some(_), None) => None,
    }
}

/// The type a receiver is declared on: `s *Server` → `Server`, `Engine` →
/// `Engine`, `m *Map[K, V]` → `Map`.
fn receiver_type(receiver: &str) -> Option<String> {
    // The type parameters go first, so `V` is not read as the type.
    let without_type_params = receiver.split('[').next().unwrap_or_default();
    let type_word = without_type_params.split_whitespace().last()?;
    next_ident(Some(type_word.trim_start_matches('*')))
}

/// `X struct` → `Struct`, `X interface` → `Trait`, `X = Y` and `X Y` → `Const`;
/// a `type ( … )` group is an anonymous `Const`, split into its types.
fn type_header(rest: &str) -> (ChunkKind, Option<String>) {
    let rest = rest.trim_start();
    if rest.starts_with('(') {
        return (ChunkKind::Const, None);
    }
    let name_len: usize = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum();
    if name_len == 0 {
        return (ChunkKind::Const, None);
    }
    let name = Some(rest[..name_len].to_string());
    let mut after = &rest[name_len..];
    // Type parameters sit glued to the name — `List[T any] struct` — while a
    // slice type is set apart by a space: `type IDs []int`.
    if after.starts_with('[')
        && let Some(past) = balanced_end(after.as_bytes(), 0, &Lexicon::GO, &[(b'[', b']')])
    {
        after = &after[past..];
    }
    let kind = match after.trim_start().as_bytes() {
        after if opens_on(after, "struct") => ChunkKind::Struct,
        after if opens_on(after, "interface") => ChunkKind::Trait,
        _ => ChunkKind::Const,
    };
    (kind, name)
}

/// The first name a `const`/`var` declares: `X = 1` → `X`; `( /* doc */ A =
/// iota … )` → `A`. The blank identifier names nothing: `var _ io.Reader =
/// (*T)(nil)` is anonymous, and `( _ = iota; A … )` is named `A`.
///
/// INTENT: a group is named after its first member rather than left
/// anonymous, because an anonymous chunk is merged into its neighbours and
/// would lose its `Const` kind with them.
fn group_or_name(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let Some(mut members) = rest.strip_prefix('(') else {
        return named(next_ident(rest.split_whitespace().next()));
    };
    while !members.is_empty() {
        let (_, code) = split_trivia(members, &Lexicon::GO);
        if let Some(name) = named(next_ident(code.split_whitespace().next())) {
            return Some(name);
        }
        let Some(eol) = code.find('\n') else {
            break;
        };
        members = &code[eol + 1..];
    }
    None
}

/// `name`, unless it is Go's blank identifier.
fn named(name: Option<String>) -> Option<String> {
    name.filter(|n| n != "_")
}

/// An interface method: an identifier directly followed by its parameters.
fn member_name(header: &str) -> Option<String> {
    let name = next_ident(Some(header))?;
    header[name.len()..].starts_with('(').then_some(name)
}

/// `TestParse`, `Test`, `Example_suffix`: what `go test` runs. `Testify` is a
/// function, not a test — the character after the prefix must not be lowercase.
fn is_test_name(name: &str) -> bool {
    TEST_PREFIXES.iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_lowercase()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_at(path: &str, src: &str, max_chars: usize) -> Vec<RawChunk> {
        let cfg = ChunkConfig {
            min_chars: 1,
            max_chars,
            ..ChunkConfig::default()
        };
        GoChunker.chunk(path, src, &cfg).expect("chunking succeeds")
    }

    fn chunk(src: &str) -> Vec<RawChunk> {
        chunk_at("net/retry.go", src, ChunkConfig::default().max_chars)
    }

    fn kinds(out: &[RawChunk]) -> Vec<(ChunkKind, Option<&str>)> {
        out.iter().map(|c| (c.kind, c.symbol.as_deref())).collect()
    }

    #[test]
    fn every_declaration_gets_its_kind_without_a_semicolon() {
        let out = chunk(
            "type Server struct {\n\taddr string\n}\n\n\
             type Handler interface {\n\tServe(w Writer) error\n}\n\n\
             type ID = string\n\ntype IDs []int\n\n\
             type List[T any] struct {\n\titems []T\n}\n\n\
             const MaxRetries = 3\n\n\
             var (\n\t// the default\n\tDefaultAddr = \":80\"\n\tother = 1\n)\n\n\
             func Run(addr string) error {\n\treturn nil\n}\n\n\
             func (s *Server) Handle(w Writer) {\n}\n\n\
             func (l *List[T]) Push(v T) {}\n\n\
             func (m *Map[K, V]) Get(k K) V {\n\treturn m.x\n}\n\n\
             func Map[T, U any](xs []T, f func(T) U) []U {\n\treturn nil\n}\n",
        );
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Struct, Some("Server")),
                (ChunkKind::Trait, Some("Handler")),
                (ChunkKind::Const, Some("ID")),
                (ChunkKind::Const, Some("IDs")),
                (ChunkKind::Struct, Some("List")),
                (ChunkKind::Const, Some("MaxRetries")),
                (ChunkKind::Const, Some("DefaultAddr")),
                (ChunkKind::Function, Some("Run")),
                (ChunkKind::Method, Some("Server.Handle")),
                (ChunkKind::Method, Some("List.Push")),
                (ChunkKind::Method, Some("Map.Get")),
                (ChunkKind::Function, Some("Map")),
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn package_and_imports_stay_anonymous_and_merge() {
        let out = GoChunker
            .chunk(
                "main.go",
                "// Package main boots.\npackage main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {}\n",
                &ChunkConfig::default(),
            )
            .expect("chunking succeeds");
        assert_eq!(out.len(), 2, "got {out:#?}");
        assert!(out[0].symbol.is_none());
        assert!(out[0].text.starts_with("// Package main boots."));
        assert!(out[0].text.contains("\"os\""));
        assert_eq!(out[1].symbol.as_deref(), Some("main"));
    }

    #[test]
    fn a_doc_comment_rides_with_its_declaration() {
        let out = chunk("// Run starts the loop.\n// It blocks.\nfunc Run() {\n}\n");
        assert_eq!(out.len(), 1);
        assert!(out[0].text.starts_with("// Run starts the loop."));
        assert_eq!((out[0].start_line, out[0].end_line), (1, 4));
    }

    #[test]
    fn test_functions_are_tests_only_in_a_test_file() {
        let src = "func TestParse(t *testing.T) {}\n\nfunc BenchmarkParse(b *testing.B) {}\n\nfunc Testify() {}\n\nfunc helper() {}\n";
        let out = chunk_at("parse_test.go", src, 2400);
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Test, Some("TestParse")),
                (ChunkKind::Test, Some("BenchmarkParse")),
                (ChunkKind::Function, Some("Testify")),
                (ChunkKind::Function, Some("helper")),
            ]
        );
        let out = chunk_at("parse.go", src, 2400);
        assert_eq!(out[0].kind, ChunkKind::Function);

        let strict = ChunkConfig {
            min_chars: 1,
            include_tests: false,
            ..ChunkConfig::default()
        };
        let kept = GoChunker
            .chunk("parse_test.go", src, &strict)
            .expect("chunking succeeds");
        assert_eq!(kinds(&kept).len(), 2, "got {kept:#?}");
    }

    #[test]
    fn a_large_interface_splits_into_its_methods() {
        let src = "type Store interface {\n\t// Get reads one.\n\tGet(key string) ([]byte, error)\n\tPut(key string, v []byte) error\n\tio.Closer\n}\n";
        let out = chunk_at("store.go", src, 60);
        let get = out
            .iter()
            .find(|c| c.symbol.as_deref() == Some("Get"))
            .expect("Get");
        assert_eq!(get.kind, ChunkKind::Method);
        assert_eq!(get.breadcrumb.as_deref(), Some("Store > Get"));
        assert!(get.text.starts_with("// Get reads one."));
        assert!(out.iter().any(|c| c.symbol.as_deref() == Some("Put")));
    }

    #[test]
    fn braces_in_raw_strings_runes_and_strings_do_not_close_a_function() {
        let src = "func a() string {\n\tr := '{'\n\tq := `\n}\n`\n\treturn \"}\" + q + string(r)\n}\n\nfunc b() {}\n";
        assert_eq!(
            kinds(&chunk(src)),
            vec![
                (ChunkKind::Function, Some("a")),
                (ChunkKind::Function, Some("b"))
            ]
        );
    }

    #[test]
    fn an_unterminated_string_does_not_swallow_the_next_function() {
        let out = chunk("func a() {\n\ts := \"open {\n}\n\nfunc b() {}\n");
        assert!(
            out.iter().any(|c| c.symbol.as_deref() == Some("b")),
            "got {out:#?}"
        );
    }

    #[test]
    fn garbage_still_yields_sane_fragments() {
        let out = chunk("func (\n)) ]] type ( {{ `\n");
        for c in &out {
            assert!(c.end_line >= c.start_line, "got {c:?}");
        }
    }

    #[test]
    fn a_brace_closing_a_type_literal_does_not_end_the_function() {
        let shapes = [
            "func Get(key string) interface{} {\n\treturn nil\n}\n",
            "func Get() <-chan struct{} {\n\treturn nil\n}\n",
            "func Get() map[string]interface{} {\n\treturn nil\n}\n",
        ];
        for src in shapes {
            let out = chunk(&format!("{src}\nfunc after() {{}}\n"));
            assert_eq!(
                kinds(&out),
                vec![
                    (ChunkKind::Function, Some("Get")),
                    (ChunkKind::Function, Some("after"))
                ],
                "{src:?} gave {out:#?}"
            );
            assert_eq!((out[0].start_line, out[0].end_line), (1, 3), "{src:?}");
        }
        let out = chunk("var cfg = struct{ A int }{A: 1}\n\nfunc after() {}\n");
        assert_eq!(out[0].symbol.as_deref(), Some("cfg"), "got {out:#?}");
        assert!(out[0].text.ends_with("{A: 1}"), "got {out:#?}");
    }

    #[test]
    fn a_type_group_declares_each_of_its_types() {
        let src =
            "// The shapes.\ntype (\n\tA struct{ x int }\n\tB interface{ M() }\n\tID = string\n)\n";
        let out = chunk(src);
        let typed: Vec<_> = kinds(&out)
            .into_iter()
            .filter(|(_, s)| s.is_some())
            .collect();
        assert_eq!(
            typed,
            vec![
                (ChunkKind::Struct, Some("A")),
                (ChunkKind::Trait, Some("B")),
                (ChunkKind::Const, Some("ID")),
            ],
            "got {out:#?}"
        );
        assert_eq!(named_chunk(&out, "B").breadcrumb.as_deref(), Some("B"));
        assert!(
            out[0].text.starts_with("// The shapes.\ntype ("),
            "got {out:#?}"
        );
    }

    #[test]
    fn a_unicode_type_name_is_read_whole() {
        let out = chunk("type Größe struct {\n\tv int\n}\n");
        assert_eq!(kinds(&out), vec![(ChunkKind::Struct, Some("Größe"))]);
    }

    #[test]
    fn a_test_file_is_one_whatever_the_case_of_its_extension() {
        let out = chunk_at("cmd/main_test.GO", "func TestMain(t *testing.T) {}\n", 2400);
        assert_eq!(out[0].kind, ChunkKind::Test);
    }

    fn named_chunk<'a>(out: &'a [RawChunk], symbol: &str) -> &'a RawChunk {
        out.iter()
            .find(|c| c.symbol.as_deref() == Some(symbol))
            .unwrap_or_else(|| panic!("no `{symbol}` in {out:#?}"))
    }

    #[test]
    fn the_blank_identifier_and_a_comment_name_no_group() {
        let out = chunk(
            "var _ io.Reader = (*T)(nil)\n\n\
             const (\n\t/* The modes,\n\t * in order. */\n\t_ = iota\n\tFast\n\tSlow\n)\n",
        );
        assert_eq!(
            kinds(&out),
            vec![(ChunkKind::Const, None), (ChunkKind::Const, Some("Fast"))],
            "got {out:#?}"
        );
    }
}
