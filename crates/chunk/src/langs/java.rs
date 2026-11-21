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
use crate::header::{
    annotation_names, header_of, is_test_annotation, next_ident, split_trivia, strip_annotations,
};
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

    fn classify(&self, item: &str, nested: bool) -> ItemMeta {
        let lex = self.lexicon();
        let (trivia, code) = split_trivia(item, lex);
        let is_test = annotation_names(trivia, lex)
            .into_iter()
            .any(is_test_annotation);
        let header = header_of(code, lex, b"{;=");
        let has_body = code[header.len()..].starts_with('{');
        let (kind, symbol) = parse_header(&strip_annotations(header, lex), nested, has_body);
        ItemMeta {
            kind,
            symbol,
            is_test,
        }
    }

    fn recursable(&self, kind: ChunkKind) -> bool {
        matches!(kind, ChunkKind::Struct | ChunkKind::Trait | ChunkKind::Enum)
    }
}

/// Reads the kind and the symbol out of a header (annotations stripped, no
/// body, no initializer). `has_body` says a `{` follows it.
fn parse_header(header: &str, nested: bool, has_body: bool) -> (ChunkKind, Option<String>) {
    let has_params = header.contains('(');
    // Only what precedes the parameter list can declare: in `void save(Record
    // record)` the `record` is a parameter, not a record.
    let before = header.split('(').next().unwrap_or(header);
    let cleaned = before.replace(['<', '>', ',', '[', ']', '?', '&'], " ");
    let toks: Vec<&str> = cleaned.split_whitespace().collect();
    for (k, tok) in toks.iter().enumerate() {
        let Some((_, kind)) = TYPE_KEYWORDS.iter().find(|(kw, _)| kw == tok) else {
            continue;
        };
        let name = next_ident(toks.get(k + 1).copied());
        // `record` is a contextual keyword: a field may be named `record`,
        // while a real record always carries its component list.
        if *tok == "record" && (name.is_none() || !has_params) {
            break;
        }
        return (*kind, name);
    }
    let name = toks.last().and_then(|t| next_ident(Some(t)));
    if has_params {
        // A member with a parameter list is a method or a constructor. An enum
        // constant — `RED("r"),`, `Mercury(3.3e23),` — has the same bare shape:
        // it is told apart by having no body (a constructor always has one),
        // or, when it carries a body of its own, by its SCREAMING name.
        let bare = toks.len() == 1;
        let constant = bare && (!has_body || name.as_deref().is_some_and(is_screaming));
        if nested && !constant {
            return (ChunkKind::Method, name);
        }
        return (ChunkKind::Text, None);
    }
    if toks.contains(&"static") && toks.contains(&"final") {
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
