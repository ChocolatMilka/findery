//! Reading an item's header: where its trivia stops, which annotations it
//! carries, and the name its keyword gives it.
//!
//! INTENT: every brace-delimited profile asks the same three questions of an
//! item before it can classify it — what is trivia, what is header, what is
//! the name — and the answers depend only on the item's text and its
//! [`Lexicon`]. Holding them apart from the pipeline in [`crate::brace`] keeps
//! that pipeline about splitting, and these helpers about reading.

use findery_core::model::ChunkKind;

use crate::scanner::Lexicon;

/// Returns `(trivia, code)`: everything up to the first byte that is not a
/// comment, an attribute, a decorator or whitespace, and the rest.
///
/// INTENT: a scanner, not a line loop. Several attributes may share a line with
/// each other and with the item they document — `#[ignore] #[test] fn t() {}` is
/// one item, tested, named `t` — and only a byte-wise walk gets that right. The
/// trivia is read for the item's `kind`, its symbol and its `test` flag; the
/// item's TEXT is its whole span either way.
pub(crate) fn split_trivia<'a>(item: &'a str, lex: &Lexicon) -> (&'a str, &'a str) {
    let bytes = item.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if lex.attributes && starts_with(bytes, i, b"#[") {
            match balanced_end(bytes, i + 1, lex, ATTRIBUTE_BRACKETS) {
                Some(end) => i = end,
                // An attribute that never closes swallows the rest of the item.
                None => return (item, &item[item.len()..]),
            }
            continue;
        }
        if lex.attributes && starts_with(bytes, i, b"#![") {
            match balanced_end(bytes, i + 2, lex, ATTRIBUTE_BRACKETS) {
                Some(end) => i = end,
                None => return (item, &item[item.len()..]),
            }
            continue;
        }
        if lex.decorators && b == b'@' {
            // `@interface Foo` declares: it is code, not an annotation.
            if lex
                .at_keywords
                .contains(&&item[i + 1..annotation_name_end(bytes, i)])
            {
                break;
            }
            i = decorator_end(bytes, i, lex);
            continue;
        }
        // Before the line comments is impossible here: `#[…]` was handled
        // above, and every other `#` really is a comment.
        let is_comment = lex
            .line_comments
            .iter()
            .any(|open| starts_with(bytes, i, open.as_bytes()))
            || lex
                .block_comment
                .as_ref()
                .is_some_and(|bc| starts_with(bytes, i, bc.open.as_bytes()));
        if is_comment && let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.min(bytes.len());
            continue;
        }
        break;
    }
    (&item[..i], &item[i..])
}

/// Whether `needle` sits at `i`.
fn starts_with(bytes: &[u8], i: usize, needle: &[u8]) -> bool {
    bytes[i..].starts_with(needle)
}

/// The pairs an attribute's argument list is written with.
const ATTRIBUTE_BRACKETS: &[(u8, u8)] = &[(b'[', b']')];

/// The pairs a decorator's argument list is written with.
const DECORATOR_BRACKETS: &[(u8, u8)] = &[(b'(', b')'), (b'[', b']'), (b'{', b'}')];

/// A body's braces.
pub(crate) const BRACES: &[(u8, u8)] = &[(b'{', b'}')];

/// A parameter or a group list.
pub(crate) const PARENS: &[(u8, u8)] = &[(b'(', b')')];

/// The offset just past the construct opened at `start`, or `None` when it
/// never closes before the end of the input.
///
/// INTENT: the one bracket matcher of the crate. The count runs through
/// [`crate::scanner::skip_opaque`], so a bracket inside a string —
/// `@Route('/', methods: ['GET'])`, `#[doc = "a ] b"]` — is not a bracket at
/// all, and a list may span several lines.
pub(crate) fn balanced_end(
    bytes: &[u8],
    start: usize,
    lex: &Lexicon,
    pairs: &[(u8, u8)],
) -> Option<usize> {
    let mut i = start;
    let mut depth = 0isize;
    while i < bytes.len() {
        if let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.min(bytes.len());
            continue;
        }
        if pairs.iter().any(|(open, _)| *open == bytes[i]) {
            depth += 1;
        } else if pairs.iter().any(|(_, close)| *close == bytes[i]) {
            depth -= 1;
            if depth <= 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// The offset just past the name of the annotation whose `@` sits at `i`:
/// `@Test`, `@org.junit.Test`, `@file:JvmName`, `@field:Json`.
///
/// INTENT: one spelling of an annotation name for every reader of one — the
/// trivia scan, the test-flag scan, the header stripper, the splitter — so a
/// use-site target (`file:`, `field:`) is never half-read by one of them.
pub(crate) fn annotation_name_end(bytes: &[u8], i: usize) -> usize {
    let mut end = i + 1;
    while let Some(&c) = bytes.get(end) {
        let target = c == b':' && bytes.get(end + 1).is_some_and(u8::is_ascii_alphabetic);
        if !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'.') || target) {
            break;
        }
        end += 1;
    }
    end
}

/// The offset just past a decorator starting at `@`: its name, then its
/// argument list when it has one. `@sealed class X` is a decorator followed by
/// code, not two lines of trivia.
pub(crate) fn decorator_end(bytes: &[u8], i: usize, lex: &Lexicon) -> usize {
    let name = annotation_name_end(bytes, i);
    if name == i + 1 {
        return name; // a lone `@`: not a decorator, let the caller decide
    }
    let mut args = name;
    while bytes
        .get(args)
        .is_some_and(|c| c.is_ascii_whitespace() && *c != b'\n')
    {
        args += 1;
    }
    match bytes.get(args) {
        Some(b'(' | b'[' | b'{') => {
            balanced_end(bytes, args, lex, DECORATOR_BRACKETS).unwrap_or(bytes.len())
        }
        _ => name,
    }
}

/// Reads a kind and a symbol out of a header using a keyword table.
///
/// INTENT: the token AFTER the keyword names the item. Scanning the header for
/// the first keyword — rather than parsing it — is what makes `export default
/// async function`, `public static function` and `abstract class` one rule.
pub(crate) fn keyword_header(
    header: &str,
    table: &[(&str, ChunkKind)],
) -> (ChunkKind, Option<String>) {
    // Generic parameter lists are blown apart so that `impl<'a>` tokenizes as
    // `impl`, and `struct Foo<T>` as `struct` + `Foo`.
    let cleaned = header.replace(['(', ')', ',', '<', '>'], " ");
    let mut toks = cleaned.split_whitespace();
    while let Some(tok) = toks.next() {
        if let Some((_, kind)) = table.iter().find(|(kw, _)| *kw == tok) {
            return (*kind, next_ident(toks.next()));
        }
    }
    (ChunkKind::Text, None)
}

/// Keeps the identifier part of a token: `Foo<T>` -> `Foo`, `X:` -> `X`.
///
/// A backslash belongs to it and to nothing else: `namespace App\Http` is one
/// name, and no other language puts a `\` in an identifier.
pub(crate) fn next_ident(tok: Option<&str>) -> Option<String> {
    let raw = tok?;
    let ident: String = raw
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '\\')
        .collect();
    if ident.is_empty() { None } else { Some(ident) }
}

/// The header of an item: its code up to the first of `stops` that sits
/// outside every `(…)` and `[…]`, strings and comments skipped.
///
/// INTENT: a parameter list may hold a default (`x: Int = 3`), a lambda
/// (`{ … }`) or an annotation argument (`@Size(min = 1)`); cutting at the first
/// `=` or `{` byte would cut the header inside it and lose the name.
pub(crate) fn header_of<'a>(code: &'a str, lex: &Lexicon, stops: &[u8]) -> &'a str {
    let bytes = code.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.max(i + 1);
            continue;
        }
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b if depth == 0 && stops.contains(&b) => return &code[..i],
            _ => {}
        }
        i += 1;
    }
    code
}

/// The simple names of the annotations in `trivia`: `@Test` → `Test`,
/// `@org.junit.jupiter.api.Test` → `Test`. Comments are skipped, so a javadoc
/// `@param` is not an annotation.
pub(crate) fn annotation_names<'a>(trivia: &'a str, lex: &Lexicon) -> Vec<&'a str> {
    let bytes = trivia.as_bytes();
    let mut names = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.max(i + 1);
            continue;
        }
        if bytes[i] == b'@' {
            let end = annotation_name_end(bytes, i);
            let full = &trivia[i + 1..end];
            names.push(full.rsplit('.').next().unwrap_or(full));
            i = end.max(i + 1);
            continue;
        }
        i += 1;
    }
    names
}

/// `header` without the annotations written inside it — `public @Deprecated(since
/// = "9") class Foo` reads as `public  class Foo` — so that a parenthesis or
/// an `=` of an annotation is never taken for the item's own. The lexicon's
/// `at_keywords` stay: `@interface` is syntax, not an annotation.
pub(crate) fn strip_annotations(header: &str, lex: &Lexicon) -> String {
    let bytes = header.as_bytes();
    let mut out = String::with_capacity(header.len());
    let mut from = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = crate::scanner::skip_opaque(bytes, i, lex) {
            i = next.max(i + 1);
            continue;
        }
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let mut end = annotation_name_end(bytes, i);
        if lex.at_keywords.contains(&&header[i + 1..end]) {
            i = end;
            continue;
        }
        if bytes.get(end) == Some(&b'(') {
            end = balanced_end(bytes, end, lex, PARENS).unwrap_or(bytes.len());
        }
        out.push_str(&header[from..i]);
        out.push(' ');
        from = end;
        i = end;
    }
    out.push_str(&header[from.min(header.len())..]);
    out
}

/// The simple names of the annotations that mark a test method: JUnit 4 and 5,
/// TestNG, jqwik.
const TEST_MARKERS: &[&str] = &[
    "Test",
    "ParameterizedTest",
    "RepeatedTest",
    "TestFactory",
    "TestTemplate",
    "Theory",
    "Property",
];

/// The framework test slices that make a whole class a test.
const TEST_SLICES: &[&str] = &[
    "SpringBootTest",
    "WebMvcTest",
    "WebFluxTest",
    "DataJpaTest",
    "DataJdbcTest",
    "DataMongoTest",
    "DataRedisTest",
    "JdbcTest",
    "JsonTest",
    "RestClientTest",
    "GraphQlTest",
    "QuarkusTest",
    "MicronautTest",
    "HiltAndroidTest",
];

/// Whether an annotation marks a test.
///
/// INTENT: an exact set, not a suffix: TestNG's `@BeforeTest` and `@AfterTest`
/// set a test up, they are not one, and `include_tests = false` must keep them.
pub(crate) fn is_test_annotation(name: &str) -> bool {
    TEST_MARKERS.contains(&name) || TEST_SLICES.contains(&name)
}

/// What a Java or Kotlin item says once its annotations are read.
pub(crate) struct Annotated<'a> {
    /// One of its annotations marks a test.
    pub is_test: bool,
    /// Its header, up to the first of the stops, annotations stripped.
    pub header: String,
    /// The code after the header: `{…}`, `= …`, `;`, or nothing.
    pub rest: &'a str,
}

/// Reads an annotated item: its test flag, its header, and what follows it.
pub(crate) fn annotated_header<'a>(item: &'a str, lex: &Lexicon, stops: &[u8]) -> Annotated<'a> {
    let (trivia, code) = split_trivia(item, lex);
    let is_test = annotation_names(trivia, lex)
        .into_iter()
        .any(is_test_annotation);
    let raw = header_of(code, lex, stops);
    Annotated {
        is_test,
        header: strip_annotations(raw, lex),
        rest: &code[raw.len()..],
    }
}

/// Whether `rest` opens on `token` — on the whole word, when `token` is a word.
pub(crate) fn opens_on(rest: &[u8], token: &str) -> bool {
    let token = token.as_bytes();
    let is_word = token.first().is_some_and(u8::is_ascii_alphabetic);
    let glued = rest
        .get(token.len())
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    rest.starts_with(token) && !(is_word && glued)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivia_stops_at_the_first_line_of_code() {
        let item = "/// Doc.\n// plain\n#[derive(Debug)]\nstruct S {\n    a: u8,\n}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert!(trivia.ends_with("#[derive(Debug)]\n"));
        assert!(code.starts_with("struct S"));
    }

    #[test]
    fn a_php_hash_comment_is_trivia_but_an_attribute_is_too() {
        let item = "# a comment\n#[Attr]\nclass C {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::PHP);
        assert!(trivia.contains("# a comment"));
        assert!(trivia.contains("#[Attr]"));
        assert!(code.starts_with("class C"));
    }

    #[test]
    fn a_decorator_with_braced_arguments_stays_trivia() {
        let item = "@Component({\n    selector: 'app-widget',\n    template: 'a ] b'\n})\nexport class Widget {\n    render() {}\n}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::JAVASCRIPT);
        assert!(trivia.contains("selector"), "got {trivia:?}");
        assert!(trivia.contains(')'), "got {trivia:?}");
        assert!(code.starts_with("export class Widget"), "got {code:?}");
    }

    #[test]
    fn an_attribute_sharing_its_line_with_the_item_does_not_eat_it() {
        // `#[inline] fn f() {}`: the attribute is trivia, the declaration that
        // follows it is not — reading the whole line as trivia loses the item.
        let item = "#[inline]\nfn a() {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert_eq!(trivia, "#[inline]\n");
        assert!(code.starts_with("fn a"), "got {code:?}");

        let item = "#[inline] pub fn a() { 1 }\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert_eq!(trivia.trim_end(), "#[inline]");
        assert!(code.trim_start().starts_with("pub fn a"), "got {code:?}");

        let item = "@Component() class Widget {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::JAVASCRIPT);
        assert_eq!(trivia.trim_end(), "@Component()");
        assert!(
            code.trim_start().starts_with("class Widget"),
            "got {code:?}"
        );
    }

    #[test]
    fn every_trivia_of_a_shared_line_is_read() {
        // The test flag is read from the trivia, so a second attribute sharing
        // the line must be trivia too — otherwise `include_tests = false` keeps
        // a test it promised to drop.
        let (trivia, code) = split_trivia("#[ignore] #[test] fn t() {}\n", &Lexicon::RUST);
        assert!(trivia.contains("#[ignore]"), "got {trivia:?}");
        assert!(trivia.contains("#[test]"), "got {trivia:?}");
        assert!(code.trim_start().starts_with("fn t"), "got {code:?}");

        // A decorator with no argument list ends at its name.
        let (trivia, code) = split_trivia("@sealed class Foo {}\n", &Lexicon::JAVASCRIPT);
        assert_eq!(trivia.trim_end(), "@sealed");
        assert!(code.trim_start().starts_with("class Foo"), "got {code:?}");
    }

    #[test]
    fn a_multi_line_attribute_counts_no_bracket_inside_a_string() {
        let item = "#[doc = \"a ] b\"]\nfn a() {}\n";
        let (trivia, code) = split_trivia(item, &Lexicon::RUST);
        assert!(trivia.contains("doc"), "got {trivia:?}");
        assert!(code.starts_with("fn a"), "got {code:?}");
    }

    #[test]
    fn the_keyword_after_the_modifiers_names_the_item() {
        let table = &[
            ("function", ChunkKind::Function),
            ("class", ChunkKind::Struct),
        ];
        assert_eq!(
            keyword_header("export default async function boot ", table),
            (ChunkKind::Function, Some("boot".into()))
        );
        assert_eq!(
            keyword_header("public static function render ", table),
            (ChunkKind::Function, Some("render".into()))
        );
    }
}
