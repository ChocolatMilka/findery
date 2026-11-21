//! Kotlin chunker: one chunk per top-level declaration, one per member inside
//! a class, an interface or an object big enough to be split.
//!
//! INTENT: Kotlin rides the shared pipeline of [`crate::brace`] with a lexicon
//! of its own — newline-ended statements, `"""raw strings"""`, `${…}`
//! templates followed into, nested block comments, backticked identifiers —
//! so a brace inside a string never counts and `val a = 1` on one line is an
//! item of its own without a `;`. What is Kotlin's own is said here: the soft
//! keywords (`enum class`, `companion object`, `fun interface`) and the
//! extension receiver, which belongs to the name — `fun String.slug()` is
//! found as `String.slug`.

use crate::brace::{ItemMeta, Profile, chunk_with};
use crate::header::{
    annotation_names, header_of, is_test_annotation, next_ident, split_trivia, strip_annotations,
};
use crate::scanner::Lexicon;
use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

/// Splits Kotlin source and scripts into items.
pub struct KotlinChunker;

impl Chunker for KotlinChunker {
    fn lang(&self) -> Lang {
        Lang::Kotlin
    }

    fn chunk(&self, path: &str, text: &str, cfg: &ChunkConfig) -> Result<Vec<RawChunk>> {
        chunk_with(&KotlinProfile, path, text, cfg)
    }
}

/// The name Kotlin gives a companion object that does not name itself.
const DEFAULT_COMPANION: &str = "Companion";

struct KotlinProfile;

impl Profile for KotlinProfile {
    fn lexicon(&self) -> &'static Lexicon {
        &Lexicon::KOTLIN
    }

    fn classify(&self, item: &str, nested: bool) -> ItemMeta {
        let lex = self.lexicon();
        let (trivia, code) = split_trivia(item, lex);
        let is_test = annotation_names(trivia, lex)
            .into_iter()
            .any(is_test_annotation);
        let header = strip_annotations(header_of(code, lex, b"{=;"), lex);
        let (kind, symbol) = parse_header(&header, nested);
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

/// Reads the kind and the symbol out of a header. Modifiers — `private`,
/// `data`, `sealed`, `inline`, `suspend`, `override`… — are walked past: the
/// keyword after them names the item.
fn parse_header(header: &str, nested: bool) -> (ChunkKind, Option<String>) {
    let spans = word_spans(header);
    let words: Vec<&str> = spans.iter().map(|(_, w)| *w).collect();
    for (k, &(at, word)) in spans.iter().enumerate() {
        let next = words.get(k + 1).copied();
        let before = &words[..k];
        // What follows the keyword, read from the header itself: a receiver,
        // a backticked name or a `:` mean nothing once split into words.
        let after = || &header[at + word.len()..];
        match word {
            "fun" if next == Some("interface") => {
                return (ChunkKind::Trait, next_ident(words.get(k + 2).copied()));
            }
            "fun" => return (ChunkKind::Function, receiver_name(after())),
            "class" if before.contains(&"enum") => return (ChunkKind::Enum, next_ident(next)),
            "class" => return (ChunkKind::Struct, next_ident(next)),
            "interface" => return (ChunkKind::Trait, next_ident(next)),
            "object" => {
                // `companion object : Factory` names nothing: what follows the
                // `:` is a supertype, not the object.
                let named = next_ident(Some(after().trim_start()));
                let symbol = match named {
                    None if before.contains(&"companion") => Some(DEFAULT_COMPANION.to_string()),
                    other => other,
                };
                return (ChunkKind::Struct, symbol);
            }
            "typealias" => return (ChunkKind::Const, next_ident(next)),
            "val" | "var" => {
                // A property of a class is a field, not a constant — unless it
                // is a `const val`, which is exactly what someone searches for.
                if nested && !before.contains(&"const") {
                    return (ChunkKind::Text, None);
                }
                return (ChunkKind::Const, receiver_name(after()));
            }
            "constructor" => return (ChunkKind::Function, Some("constructor".into())),
            _ => {}
        }
    }
    (ChunkKind::Text, None)
}

/// The words of a header with their byte offsets, cut at whitespace, `(`, `)`
/// and `:`.
fn word_spans(header: &str) -> Vec<(usize, &str)> {
    let cut = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | ':');
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in header.char_indices() {
        match (cut(c), start) {
            (true, Some(s)) => {
                out.push((s, &header[s..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &header[s..]));
    }
    out
}

/// The name of a function or a property, receiver included: `<T> List<T>.second()`
/// → `List.second`, `` `adds two numbers`() `` → `adds two numbers`,
/// `String?.orBlank` → `String.orBlank`.
///
/// INTENT: an extension is searched for by its receiver as much as by its
/// name, so the receiver stays in the symbol; type arguments and nullability
/// do not name anything and are dropped.
fn receiver_name(after: &str) -> Option<String> {
    let mut out = String::new();
    let mut angle = 0usize;
    let mut ticked = false;
    for c in after.trim_start().chars() {
        if ticked {
            if c == '`' {
                ticked = false;
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '`' => ticked = true,
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            _ if angle > 0 => {}
            // The name is one word: `val logger by lazy { … }` names `logger`.
            c if c.is_whitespace() && !out.is_empty() => break,
            '(' | ':' => break,
            c if c.is_alphanumeric() || c == '_' || c == '.' => out.push(c),
            _ => {}
        }
    }
    let out = out.trim_matches('.').to_string();
    if out.is_empty() { None } else { Some(out) }
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
        KotlinChunker
            .chunk("src/main/kotlin/app/Client.kt", src, &cfg)
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

    #[test]
    fn every_declaration_gets_its_kind_without_a_semicolon() {
        let out = chunk(
            "data class Point(val x: Int, val y: Int)\n\
             sealed class Expr\n\
             enum class Mode { FAST, SLOW }\n\
             interface Api {\n    fun send()\n}\n\
             fun interface Handler {\n    fun handle(code: Int)\n}\n\
             object Registry\n\
             typealias Callback = (Int) -> Unit\n\
             const val MAX_RETRIES = 3\n\
             val greeting: String = \"hi\"\n\
             fun boot(args: Array<String>) {\n}\n",
        );
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Struct, Some("Point")),
                (ChunkKind::Struct, Some("Expr")),
                (ChunkKind::Enum, Some("Mode")),
                (ChunkKind::Trait, Some("Api")),
                (ChunkKind::Trait, Some("Handler")),
                (ChunkKind::Struct, Some("Registry")),
                (ChunkKind::Const, Some("Callback")),
                (ChunkKind::Const, Some("MAX_RETRIES")),
                (ChunkKind::Const, Some("greeting")),
                (ChunkKind::Function, Some("boot")),
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn an_extension_function_is_named_after_its_receiver_too() {
        let out = chunk(
            "fun String.slug(): String = lowercase()\n\nfun <T> List<T>.second(): T = this[1]\n\nval String?.orBlank: String get() = this ?: \"\"\n",
        );
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Function, Some("String.slug")),
                (ChunkKind::Function, Some("List.second")),
                (ChunkKind::Const, Some("String.orBlank")),
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn package_and_imports_stay_anonymous_and_merge() {
        let out = KotlinChunker
            .chunk(
                "a.kt",
                "package app\n\nimport kotlin.math.max\nimport app.util.*\n\nfun a() = 1\n",
                &ChunkConfig::default(),
            )
            .expect("chunking succeeds");
        assert_eq!(out.len(), 2, "got {out:#?}");
        assert!(out[0].symbol.is_none());
        assert!(out[0].text.contains("import app.util.*"));
        assert_eq!(out[1].symbol.as_deref(), Some("a"));
    }

    #[test]
    fn a_large_class_splits_into_members_with_their_docs_and_annotations() {
        let filler = "        // padding padding padding padding padding padding\n".repeat(5);
        let src = format!(
            "class Client(private val base: String) : Api {{\n    val retries = 3\n\n\
             \x20   /** Sends one. */\n    @Throws(IOException::class)\n    override suspend fun send(body: String): String {{\n{filler}        return body\n    }}\n\n\
             \x20   companion object {{\n        const val TIMEOUT = 30\n    }}\n\n\
             \x20   constructor() : this(\"x\")\n}}\n"
        );
        let out = chunk_under(&src, 300);
        let send = named(&out, "send");
        assert_eq!(send.kind, ChunkKind::Method);
        assert_eq!(send.breadcrumb.as_deref(), Some("Client > send"));
        assert!(send.text.starts_with("/** Sends one. */"), "got {send:?}");
        assert!(send.text.contains("@Throws"));
        assert_eq!(named(&out, "Companion").kind, ChunkKind::Struct);
        assert_eq!(named(&out, "constructor").kind, ChunkKind::Method);
        // A property of a class is not a constant.
        assert!(!out.iter().any(|c| c.symbol.as_deref() == Some("retries")));
    }

    #[test]
    fn a_test_annotation_tags_the_function_and_keeps_its_backticked_name() {
        let src = "class ClientTest {\n    @Test\n    fun `it's sent once`() {\n        assertEquals(1, 1)\n    }\n\n    fun helper() {}\n}\n";
        let out = chunk_under(src, 40);
        let t = named(&out, "it's sent once");
        assert_eq!(t.kind, ChunkKind::Test);
        assert!(t.text.starts_with("@Test"));
        assert_eq!(named(&out, "helper").kind, ChunkKind::Method);
    }

    #[test]
    fn braces_in_templates_raw_strings_and_chars_do_not_close_an_item() {
        let src = "fun a(m: Map<String, String>): String {\n    val c = '}'\n    val r = \"\"\"\n      { ${m[\"}\"]} \n    \"\"\"\n    return \"${m[\"{\"]} }\"\n}\n\nfun b() = \"}\"\n";
        let out = chunk(src);
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Function, Some("a")),
                (ChunkKind::Function, Some("b"))
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn a_header_spread_over_lines_stays_one_item() {
        let out = chunk(
            "class Repo(\n    private val db: Db,\n) : Base(db),\n    Closeable {\n}\n\nfun x() =\n    42\n",
        );
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Struct, Some("Repo")),
                (ChunkKind::Function, Some("x"))
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn an_unterminated_raw_string_keeps_every_item_before_it() {
        let out = chunk("fun a() = 1\n\nval s = \"\"\"never closed {\nfun b() {}\n");
        assert_eq!(out[0].symbol.as_deref(), Some("a"), "got {out:#?}");
        for c in &out {
            assert!(c.end_line >= c.start_line);
        }
    }

    #[test]
    fn a_delegated_property_is_named_without_its_delegate() {
        let out = chunk(
            "val Context.dataStore by preferencesDataStore(name = \"settings\")\nval logger by lazy { 1 }\n",
        );
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Const, Some("Context.dataStore")),
                (ChunkKind::Const, Some("logger")),
            ],
            "got {out:#?}"
        );
    }

    #[test]
    fn a_use_site_target_is_part_of_the_annotation() {
        // `@field:Json(…)` is one annotation: read as `@field` it left
        // `:Json(…)` behind as code, in front of the declaration.
        let src = "@file:JvmName(\"Utils\")\npackage a\n\n@field:Json(name = \"x\") val q = 1\n";
        let out = chunk(src);
        let q = named(&out, "q");
        assert!(q.text.starts_with("@field:Json"), "got {q:?}");
        let (trivia, code) = split_trivia("@field:Json(name = \"x\") val q = 1", &Lexicon::KOTLIN);
        assert_eq!(trivia.trim_end(), "@field:Json(name = \"x\")");
        assert_eq!(code, "val q = 1");
    }
}
