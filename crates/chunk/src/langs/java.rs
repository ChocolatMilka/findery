//! Java chunker: one chunk per top-level type, one per member inside a type
//! big enough to be split.
//!
//! INTENT: Java is brace-delimited with `;`-ended statements, so it rides the
//! shared pipeline of [`crate::brace`] and only says what is Java's own. A
//! method has no keyword to announce it — `public <T> List<T> map(…)` is named
//! by the identifier before its parameter list — so a member is read from its
//! SHAPE, the way the JavaScript chunker reads a class member. Annotations and
//! javadoc are trivia riding with the declaration below them, and a JUnit
//! annotation (`@Test`, `@ParameterizedTest`, `@SpringBootTest`…) tags the
//! item a test.

use crate::brace::{ItemMeta, Profile, chunk_with};
use crate::header::{annotated_header, next_ident};
use crate::scanner::Lexicon;
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits Java source into items.
pub struct JavaChunker;

impl Chunker for JavaChunker {
    fn lang(&self) -> Lang {
        Lang::Java
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        chunk_with(&JavaProfile, path, text, cfg)
    }
}

/// The words that declare a type. `package` and `import` are left out on
/// purpose: they name nothing worth weighting, and staying anonymous is what
/// lets a run of them merge into one chunk.
const TYPE_KEYWORDS: &[(&str, ChunkKind)] = &[
    ("class", ChunkKind::Struct),
    ("interface", ChunkKind::Trait),
    ("@interface", ChunkKind::Trait),
    ("enum", ChunkKind::Enum),
    ("record", ChunkKind::Struct),
];

struct JavaProfile;

impl Profile for JavaProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::JAVA
    }

    fn classify(&self, item: &str, parent: Option<ChunkKind>) -> ItemMeta {
        let read = annotated_header(item, self.lexicon(), b"{;=");
        let header = Header::read(&read.header, read.rest.starts_with('{'));
        let (kind, symbol) = match type_declaration(&header) {
            Some(declared) => declared,
            None => member_declaration(&header, parent),
        };
        ItemMeta {
            kind,
            symbol,
            is_test: read.is_test,
        }
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(kind, ChunkKind::Struct | ChunkKind::Trait | ChunkKind::Enum)
    }
}

/// A header (annotations stripped, no body, no initializer), as its readers
/// need it.
struct Header<'a> {
    /// The words before its parameter list, if it has one: in `void
    /// save(Record record)` the `record` is a parameter, not a record.
    words: Vec<&'a str>,
    has_params: bool,
    /// A `{` follows it.
    has_body: bool,
}

impl<'a> Header<'a> {
    fn read(header: &'a str, has_body: bool) -> Self {
        let before = header.split('(').next().unwrap_or(header);
        let cut =
            |c: char| c.is_whitespace() || matches!(c, '<' | '>' | ',' | '[' | ']' | '?' | '&');
        Self {
            words: before.split(cut).filter(|w| !w.is_empty()).collect(),
            has_params: header.contains('('),
            has_body,
        }
    }

    /// The last word: what names a member.
    fn name(&self) -> Option<String> {
        next_ident(self.words.last().copied())
    }

    /// Only its name and access modifiers: `Point`, `public Point`.
    fn is_bare(&self) -> bool {
        let (_, modifiers) = self.words.split_last().unwrap_or((&"", &[]));
        modifiers
            .iter()
            .all(|w| matches!(*w, "public" | "protected" | "private"))
    }
}

/// `class X`, `interface X`, `@interface X`, `enum X`, `record X(…)`.
fn type_declaration(header: &Header<'_>) -> Option<(ChunkKind, Option<String>)> {
    let words = &header.words;
    let k = words
        .iter()
        .position(|w| TYPE_KEYWORDS.iter().any(|(kw, _)| kw == w))?;
    let (keyword, kind) = TYPE_KEYWORDS.iter().find(|(kw, _)| *kw == words[k])?;
    let name = next_ident(words.get(k + 1).copied());
    // `record` is a contextual keyword: a field may be named `record`, while a
    // real record always carries its component list.
    if *keyword == "record" && (name.is_none() || !header.has_params) {
        return None;
    }
    Some((*kind, name))
}

/// A member of a type, which no keyword announces: a method or a constructor
/// by its parameter list, a `static final` constant, or an enum constant.
///
/// INTENT: an enum constant — `RED("r"),`, `PLUS("+") { … }` — has the bare
/// shape of a constructor, and only inside an enum can it be one: there it is
/// told apart by having no body (a constructor always has one), or by its
/// SCREAMING name. In a class, `URL(String s) {}` is a constructor, and a
/// record's compact constructor `public Point { … }` has no list at all.
fn member_declaration(
    header: &Header<'_>,
    parent: Option<ChunkKind>,
) -> (ChunkKind, Option<String>) {
    let name = header.name();
    let in_enum = parent == Some(ChunkKind::Enum);
    let constant =
        header.is_bare() && (!header.has_body || name.as_deref().is_some_and(is_screaming));
    if header.has_params {
        if parent.is_some() && !(in_enum && constant) {
            return (ChunkKind::Method, name);
        }
        return (ChunkKind::Text, None);
    }
    // `static { … }` and `{ … }` are initializers, not constructors.
    let initializer = matches!(name.as_deref(), None | Some("static"));
    let compact_constructor = header.has_body && header.is_bare() && !initializer;
    if parent.is_some() && compact_constructor && !(in_enum && constant) {
        return (ChunkKind::Method, name);
    }
    if header.words.contains(&"static") && header.words.contains(&"final") {
        return (ChunkKind::Const, name);
    }
    (ChunkKind::Text, None)
}

/// `MAX_SIZE`, `RED`: the spelling Java reserves for constants.
fn is_screaming(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_under(src: &str, max_chars: usize) -> Vec<RawChunk> {
        let cfg = ChunkConfig {
            min_chars: 1,
            max_chars,
            ..ChunkConfig::default()
        };
        JavaChunker
            .chunk("src/main/java/app/Client.java", src, &cfg)
            .expect("chunking succeeds")
    }

    fn chunk(src: &str) -> Vec<RawChunk> {
        chunk_under(src, ChunkConfig::default().max_chars)
    }

    fn kinds(out: &[RawChunk]) -> Vec<(ChunkKind, Option<&str>)> {
        out.iter().map(|c| (c.kind, c.symbol.as_deref())).collect()
    }

    fn named<'a>(out: &'a [RawChunk], symbol: &str) -> &'a RawChunk {
        out.iter()
            .find(|c| c.symbol.as_deref() == Some(symbol))
            .unwrap_or_else(|| panic!("no `{symbol}` in {out:#?}"))
    }

    /// The member named `symbol`, not the header of a type of the same name.
    fn member<'a>(out: &'a [RawChunk], symbol: &str) -> &'a RawChunk {
        out.iter()
            .find(|c| c.symbol.as_deref() == Some(symbol) && c.kind == ChunkKind::Method)
            .unwrap_or_else(|| panic!("no member `{symbol}` in {out:#?}"))
    }

    #[test]
    fn every_type_declaration_gets_its_kind() {
        let out = chunk(
            "public final class Client<T> extends Base implements Api {}\n\
             interface Api { void send(); }\n\
             enum Mode { FAST, SLOW }\n\
             public record Point(int x, int y) {}\n\
             @Retention(RUNTIME)\n@interface Audited { String value() default \"\"; }\n\
             public @interface Marker {}\n",
        );
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Struct, Some("Client")),
                (ChunkKind::Trait, Some("Api")),
                (ChunkKind::Enum, Some("Mode")),
                (ChunkKind::Struct, Some("Point")),
                (ChunkKind::Trait, Some("Audited")),
                (ChunkKind::Trait, Some("Marker")),
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn package_and_imports_stay_anonymous_and_merge() {
        let cfg = ChunkConfig::default();
        let out = JavaChunker
            .chunk(
                "A.java",
                "package app;\n\nimport java.util.List;\nimport java.util.Map;\n\nclass A {}\n",
                &cfg,
            )
            .expect("chunking succeeds");
        assert_eq!(out.len(), 2, "got {out:#?}");
        assert!(out[0].symbol.is_none());
        assert!(out[0].text.contains("import java.util.Map;"));
        assert_eq!(out[1].symbol.as_deref(), Some("A"));
    }

    #[test]
    fn a_large_class_splits_into_methods_with_their_javadoc_and_annotations() {
        let filler = "        // padding padding padding padding padding padding\n".repeat(6);
        let src = format!(
            "/** The client. */\n@Service\npublic class Client {{\n    private static final int MAX_RETRIES = 3;\n\n\
             \x20   /** Builds one. */\n    public Client(String base) {{\n{filler}    }}\n\n\
             \x20   @Override\n    public <T> List<T> map(Function<T, R> f) throws IOException {{\n{filler}    }}\n\n\
             \x20   abstract void close();\n\n    static class Inner {{ }}\n}}\n"
        );
        let out = chunk_under(&src, 400);
        assert_eq!(named(&out, "MAX_RETRIES").kind, ChunkKind::Const);
        // The type's own header is a chunk: its javadoc and annotations are in
        // no member.
        let header = named(&out, "Client");
        assert_eq!(header.kind, ChunkKind::Struct);
        assert_eq!(
            header.text,
            "/** The client. */\n@Service\npublic class Client {"
        );
        let ctor = member(&out, "Client");
        assert!(ctor.text.starts_with("/** Builds one. */"), "got {ctor:?}");
        let map = named(&out, "map");
        assert_eq!(map.kind, ChunkKind::Method);
        assert_eq!(map.breadcrumb.as_deref(), Some("Client > map"));
        assert!(map.text.starts_with("@Override"), "got {map:?}");
        assert_eq!(named(&out, "close").kind, ChunkKind::Method);
        assert_eq!(named(&out, "Inner").kind, ChunkKind::Struct);
    }

    #[test]
    fn a_test_annotation_tags_the_method_and_can_exclude_it() {
        let src = "class ClientTest {\n    @Test\n    void sends() {}\n\n    @org.junit.jupiter.params.ParameterizedTest\n    void many() {}\n\n    /** @see Test */\n    void helper() {}\n}\n";
        let out = chunk_under(src, 40);
        assert_eq!(named(&out, "sends").kind, ChunkKind::Test);
        assert_eq!(named(&out, "many").kind, ChunkKind::Test);
        // A javadoc tag is a comment, not an annotation.
        assert_eq!(named(&out, "helper").kind, ChunkKind::Method);

        let strict = ChunkConfig {
            min_chars: 1,
            max_chars: 40,
            include_tests: false,
            ..ChunkConfig::default()
        };
        let kept = JavaChunker
            .chunk("A.java", src, &strict)
            .expect("chunking succeeds");
        assert!(!kept.iter().any(|c| c.symbol.as_deref() == Some("sends")));
        assert!(kept.iter().any(|c| c.symbol.as_deref() == Some("helper")));
    }

    #[test]
    fn braces_in_text_blocks_strings_and_chars_do_not_close_a_type() {
        let src = "class A {\n    String q = \"\"\"\n        { \\\"\"\" }\n        \"\"\";\n    char c = '}';\n    String s = \"}\";\n}\n\nclass B {}\n";
        let out = chunk(src);
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Struct, Some("A")),
                (ChunkKind::Struct, Some("B"))
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn a_field_named_record_is_not_a_record() {
        let out = chunk_under(
            "class A {\n    private Record record;\n    void save(Record record) {}\n}\n",
            30,
        );
        assert!(
            !out.iter().any(|c| c.symbol.as_deref() == Some("record")),
            "got {out:#?}"
        );
        assert_eq!(named(&out, "save").kind, ChunkKind::Method);
    }

    #[test]
    fn an_enum_constant_list_is_not_a_method() {
        let out = chunk_under(
            "enum Color {\n    RED(\"r\"), GREEN(\"g\");\n\n    Color(String code) {}\n}\n",
            30,
        );
        assert!(!out.iter().any(|c| c.symbol.as_deref() == Some("RED")));
        assert_eq!(member(&out, "Color").kind, ChunkKind::Method);

        // A constant list spelled in PascalCase has no body: no method either.
        let out = chunk_under(
            "enum Planet {\n    Mercury(3.3e23, 2.4e6),\n    Venus(4.8e24, 6.0e6);\n\n    Planet(double m, double r) {}\n}\n",
            30,
        );
        assert!(
            !out.iter().any(|c| c.symbol.as_deref() == Some("Mercury")),
            "got {out:#?}"
        );
        assert_eq!(member(&out, "Planet").kind, ChunkKind::Method);
    }

    #[test]
    fn a_constructor_with_a_screaming_name_is_a_constructor_outside_an_enum() {
        let out = chunk_under(
            "class URL {\n    URL(String s) {}\n\n    URL() {}\n}\n\nenum Op {\n    IDENTITY() {\n        int f() { return 1; }\n    };\n}\n",
            30,
        );
        let ctors = out
            .iter()
            .filter(|c| c.kind == ChunkKind::Method && c.symbol.as_deref() == Some("URL"))
            .count();
        assert_eq!(ctors, 2, "got {out:#?}");
        assert!(!out.iter().any(|c| c.symbol.as_deref() == Some("IDENTITY")));
    }

    #[test]
    fn a_compact_record_constructor_is_a_constructor() {
        let out = chunk_under(
            "public record Point(int x, int y) {\n    public Point {\n        if (x < 0) throw new IllegalArgumentException();\n    }\n\n    static { init(); }\n}\n",
            60,
        );
        assert_eq!(
            member(&out, "Point").breadcrumb.as_deref(),
            Some("Point > Point")
        );
        assert!(!out.iter().any(|c| c.symbol.as_deref() == Some("static")));
    }

    #[test]
    fn an_annotation_inside_the_header_does_not_hide_the_name() {
        let out = chunk("public @Deprecated(since = \"9\") class Old {}\n");
        assert_eq!(kinds(&out), vec![(ChunkKind::Struct, Some("Old"))]);
    }

    #[test]
    fn an_unterminated_string_does_not_swallow_the_next_type() {
        let out = chunk("class A {\n    String s = \"open;\n}\n\nclass B {}\n");
        assert!(
            out.iter().any(|c| c.symbol.as_deref() == Some("B")),
            "got {out:#?}"
        );
    }
}
