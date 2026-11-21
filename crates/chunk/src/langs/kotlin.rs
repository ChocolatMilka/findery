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

use findery_core::Result;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use findery_core::traits::Chunker;

use crate::brace::{ItemMeta, Profile, chunk_with};
use crate::header::annotated_header;
use crate::scanner::Lexicon;

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

    fn classify(&self, item: &str, parent: Option<ChunkKind>) -> ItemMeta {
        let read = annotated_header(item, self.lexicon(), b"{=;");
        let (kind, symbol) = parse_header(&read.header, parent.is_some());
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

/// Reads the kind and the symbol out of a header. Modifiers — `private`,
/// `data`, `sealed`, `inline`, `suspend`, `override`… — are walked past: the
/// keyword after them names the item.
fn parse_header(header: &str, nested: bool) -> (ChunkKind, Option<String>) {
    let words = word_spans(header);
    for (k, &(at, word)) in words.iter().enumerate() {
        let follows = |modifier: &str| words[..k].iter().any(|(_, w)| *w == modifier);
        // The name is read from the header itself, not from the words: a
        // receiver, a backticked name or a `:` mean nothing once split.
        let name = || receiver_name(&header[at + word.len()..]);
        match word {
            "fun" => match words.get(k + 1) {
                Some(&(next, keyword @ "interface")) => {
                    let after = &header[next + keyword.len()..];
                    return (ChunkKind::Trait, receiver_name(after));
                }
                _ => return (ChunkKind::Function, name()),
            },
            "class" if follows("enum") => return (ChunkKind::Enum, name()),
            "class" => return (ChunkKind::Struct, name()),
            "interface" => return (ChunkKind::Trait, name()),
            // `companion object : Factory` names nothing: what follows the `:`
            // is a supertype, not the object.
            "object" => match name() {
                None if follows("companion") => {
                    return (ChunkKind::Struct, Some(DEFAULT_COMPANION.to_string()));
                }
                symbol => return (ChunkKind::Struct, symbol),
            },
            "typealias" => return (ChunkKind::Const, name()),
            // A property of a class is a field, not a constant — unless it is
            // a `const val`, which is exactly what someone searches for.
            "val" | "var" if nested && !follows("const") => return (ChunkKind::Text, None),
            "val" | "var" => return (ChunkKind::Const, name()),
            "constructor" => return (ChunkKind::Function, Some("constructor".into())),
            _ => {}
        }
    }
    (ChunkKind::Text, None)
}

/// The words of a header with their byte offsets, cut at whitespace, `(`,
/// `)`, `:` and `<` — so `fun<T>` is the keyword `fun`.
fn word_spans(header: &str) -> Vec<(usize, &str)> {
    let cut = |c: char| c.is_whitespace() || matches!(c, '(' | ')' | ':' | '<');
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

/// The name a declaration gives, read from the text after its keyword,
/// receiver included: `<T> List<T>.second()` → `List.second`,
/// `String?.orBlank` → `String.orBlank`, and a backticked name keeps its
/// words: `` `adds two numbers`() `` → `adds two numbers`, `` class `My Class` ``
/// → `My Class`.
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
    fn a_primary_constructor_on_the_next_line_stays_with_its_class() {
        let body = "{\n    fun x() = 1\n}\nval after = 1\n";
        let headers = [
            "class A\n    internal actual constructor(val b: B) ",
            "expect class A\n    expect constructor(val b: B) ",
            "class A\n/** KDoc on the constructor. */\n@Inject constructor(val b: B) ",
            "class A\n    // why it is injected\n    @Inject constructor(val b: B) ",
            "class A\n    @Inject\n    constructor(val b: B) ",
            "class A private\nconstructor(val b: B) ",
            "class A\npublic constructor(val b: B) ",
        ];
        for header in headers {
            let out = chunk(&format!("{header}{body}"));
            assert_eq!(
                kinds(&out),
                vec![
                    (ChunkKind::Struct, Some("A")),
                    (ChunkKind::Const, Some("after"))
                ],
                "{header:?} gave {out:#?}"
            );
        }
    }

    #[test]
    fn a_backticked_type_name_names_its_type() {
        let src = "class `My Class` {\n    fun `do it`() = 1\n}\n\ninterface `-Iface` {\n    fun a()\n}\n";
        let out = chunk(src);
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Struct, Some("My Class")),
                (ChunkKind::Trait, Some("-Iface"))
            ],
            "got {out:#?}"
        );
        let split = chunk_under(src, 30);
        assert_eq!(
            named(&split, "do it").breadcrumb.as_deref(),
            Some("My Class > do it")
        );
    }

    #[test]
    fn an_inline_or_external_accessor_stays_with_its_property() {
        let src = "val Int.isSurrogate: Boolean\n    inline get() = this > 1\n\nval handle: Long\n    external get\n\nfun after() = 1\n";
        assert_eq!(
            kinds(&chunk(src)),
            vec![
                (ChunkKind::Const, Some("Int.isSurrogate")),
                (ChunkKind::Const, Some("handle")),
                (ChunkKind::Function, Some("after")),
            ]
        );
    }

    #[test]
    fn a_secondary_constructor_is_a_member_of_its_own() {
        let src = "class Sink {\n    private val mac: Mac?\n\n    internal constructor(a: A) : super(a) {\n        this.mac = null\n    }\n\n    internal constructor(b: B) : this(b)\n}\n";
        let out = chunk_under(src, 120);
        let constructors = out
            .iter()
            .filter(|c| c.symbol.as_deref() == Some("constructor"))
            .count();
        assert_eq!(constructors, 2, "got {out:#?}");
    }

    #[test]
    fn a_modifier_word_inside_a_qualified_name_ends_nothing() {
        let out = chunk(
            "package okio.internal\n\nimport okio.Buffer\n\ninternal val Int.isSurrogate: Boolean\n    inline get() = this > 1\n",
        );
        let named = out
            .iter()
            .find(|c| c.symbol.is_some())
            .expect("a named chunk");
        assert_eq!(named.symbol.as_deref(), Some("Int.isSurrogate"));
        assert_eq!(named.start_line, 5, "got {out:#?}");
    }

    #[test]
    fn a_type_parameter_glued_to_fun_does_not_hide_the_function() {
        let out = chunk("fun<T> id(x: T): T = x\n");
        assert_eq!(kinds(&out), vec![(ChunkKind::Function, Some("id"))]);
    }

    #[test]
    fn a_catch_or_an_else_on_the_next_line_continues_the_statement() {
        let src = "val x = try {\n    f()\n}\ncatch (e: E) {\n    0\n}\nval y = if (a) 1 else\n    2\nfun z() = 3\n";
        let out = chunk(src);
        assert_eq!(
            kinds(&out),
            vec![
                (ChunkKind::Const, Some("x")),
                (ChunkKind::Const, Some("y")),
                (ChunkKind::Function, Some("z")),
            ],
            "got {out:#?}"
        );
        assert!(named(&out, "x").text.contains("catch"));
    }

    #[test]
    fn a_half_typed_template_swallows_nothing_below_its_line() {
        let out = chunk("fun a() {\n    println(\"${x\")\n}\n\nfun b() {}\n\nfun c() {}\n");
        let names: Vec<_> = out.iter().filter_map(|c| c.symbol.as_deref()).collect();
        assert_eq!(names, vec!["a", "b", "c"], "got {out:#?}");
    }

    #[test]
    fn unclosed_templates_cost_linear_time() {
        // Every enclosing template used to rescan what the inner one had
        // already read: thirty of these lines took longer than twenty seconds.
        for line in ["\"${x\n", "\"${(x\n"] {
            let src = line.repeat(40_000);
            let started = std::time::Instant::now();
            let out = chunk(&src);
            assert!(!out.is_empty());
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{line:?} took {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn a_use_site_target_is_part_of_the_annotation() {
        // `@field:Json(…)` is one annotation: read as `@field` it left
        // `:Json(…)` behind as code, in front of the declaration.
        let src = "@file:JvmName(\"Utils\")\npackage a\n\n@field:Json(name = \"x\") val q = 1\n";
        let out = chunk(src);
        let q = named(&out, "q");
        assert!(q.text.starts_with("@field:Json"), "got {q:?}");
        let (trivia, code) =
            crate::header::split_trivia("@field:Json(name = \"x\") val q = 1", &Lexicon::KOTLIN);
        assert_eq!(trivia.trim_end(), "@field:Json(name = \"x\")");
        assert_eq!(code, "val q = 1");
    }
}
