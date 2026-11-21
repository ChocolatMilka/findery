//! The coverage invariant: chunking loses no content.
//!
//! INTENT: a chunker that splits wrongly usually fails LOUDLY — a wrong name, a
//! wrong kind, a test asserting on it. A chunker that DROPS text fails in
//! silence: the file is indexed, every chunk looks sane, and a function nobody
//! will ever find again is simply not there. This invariant is what makes that
//! failure loud, for every language at once: every word of every source line
//! is carried by a chunk whose line range covers that line.

mod fixtures;

use findery_chunk::Registry;
use findery_core::config::ChunkConfig;
use findery_core::model::{Lang, RawChunk};

/// A budget that forces every container to be split, and the default one.
const BUDGETS: &[usize] = &[120, 2400];

/// Lines a chunker drops on purpose, as `.claude/docs/chunk/README.md` says it
/// does: a JavaScript shebang and a PHP opening tag say nothing.
fn dropped_on_purpose(line_no: usize, line: &str) -> bool {
    (line_no == 1 && line.starts_with("#!")) || line.trim() == "<?php"
}

/// The words of a line: what a search could look for in it.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

/// Fails when a word of `src` is carried by no chunk covering its line.
fn assert_nothing_is_dropped(path: &str, src: &str) {
    let registry = Registry::new();
    for &max_chars in BUDGETS {
        let cfg = ChunkConfig {
            max_chars,
            include_tests: true,
            ..ChunkConfig::default()
        };
        let out = registry
            .chunk(path, Lang::from_path(path), src, &cfg)
            .expect("chunking succeeds");
        for (k, line) in src.lines().enumerate() {
            let line_no = k + 1;
            if dropped_on_purpose(line_no, line) {
                continue;
            }
            for word in words(line) {
                assert!(
                    carried(&out, line_no, word),
                    "{path} at max_chars = {max_chars}: line {line_no} `{line}` lost `{word}`\n{out:#?}"
                );
            }
        }
    }
}

/// Whether a chunk whose range covers `line_no` holds `word`.
fn carried(out: &[RawChunk], line_no: usize, word: &str) -> bool {
    out.iter().any(|c| {
        (c.start_line as usize..=c.end_line as usize).contains(&line_no) && c.text.contains(word)
    })
}

#[test]
fn every_fixture_keeps_every_word_of_every_line() {
    for (path, src) in fixtures::ALL {
        assert_nothing_is_dropped(path, src);
    }
}

#[test]
fn the_crate_own_sources_keep_every_word_of_every_line() {
    // Real files, with what fixtures never think of: test modules, raw
    // strings, long doc comments, attributes over several lines.
    let sources: &[(&str, &str)] = &[
        ("src/items.rs", include_str!("../src/items.rs")),
        ("src/brace.rs", include_str!("../src/brace.rs")),
        ("src/scanner/mod.rs", include_str!("../src/scanner/mod.rs")),
        (
            "src/langs/markdown.rs",
            include_str!("../src/langs/markdown.rs"),
        ),
        (
            "src/langs/python.rs",
            include_str!("../src/langs/python.rs"),
        ),
    ];
    for (path, src) in sources {
        assert_nothing_is_dropped(path, src);
    }
}

#[test]
fn a_brace_in_a_header_does_not_drop_the_body_it_precedes() {
    // Each of these once lost its whole body: the first `{` of the item — in
    // an annotation, a decorator, a default lambda — was taken for the body.
    let methods = "    void a() {\n        int x = 1; // padding padding padding padding\n    }\n    void b() {\n        int y = 2; // padding padding padding padding\n    }\n";
    let funs = "    fun a() {\n        println(\"padding padding padding padding\")\n    }\n    fun b() {\n        println(\"padding padding padding padding\")\n    }\n";
    let members = "  a() {\n    return 'padding padding padding padding padding';\n  }\n  b() {\n    return 'padding padding padding padding padding';\n  }\n";
    let cases = [
        (
            "A.java",
            format!(
                "@SuppressWarnings({{\"unchecked\", \"rawtypes\"}})\npublic class Client {{\n{methods}}}\n"
            ),
        ),
        (
            "a.kt",
            format!("class Button(val onClick: () -> Unit = {{ log(\"x\") }}) {{\n{funs}}}\n"),
        ),
        (
            "a.ts",
            format!("@Component({{\n  selector: 'x'\n}})\nexport class W {{\n{members}}}\n"),
        ),
    ];
    for (path, src) in &cases {
        assert_nothing_is_dropped(path, src);
    }
}

#[test]
fn the_shapes_that_once_misled_a_chunker_keep_every_word() {
    let pad = "        int x = 1; // padding padding padding padding padding\n".repeat(4);
    let kpad = "        println(\"padding padding padding padding padding\")\n".repeat(4);
    let cases: Vec<(&str, String)> = vec![
        (
            "a.kt",
            "val Context.dataStore by preferencesDataStore(name = \"settings\")\nval logger by lazy { 1 }\n".into(),
        ),
        ("a.go", "func (m *Map[K, V]) Get(k K) V {\n\treturn m.x\n}\n".into()),
        (
            "a.kt",
            "class Foo\n    @Inject constructor(val a: A) {\n    fun x() = 1\n}\n".into(),
        ),
        (
            "a.kt",
            "val x = when {\n    a -> 1\n    else -> 2\n}\nfun y() = 2\n".into(),
        ),
        ("a.kt", "val x = if (a) 1\nelse 2\nfun y() = 2\n".into()),
        (
            "a.kt",
            "@file:JvmName(\"Utils\")\npackage a\n\n@field:Json(name=\"x\") val q = 1\n".into(),
        ),
        (
            "a.go",
            "package a\r\n\r\nconst X = 1\r\n\r\nfunc F() {\r\n}\r\n".into(),
        ),
        (
            "a.go",
            "var _ io.Reader = (*T)(nil)\nvar _ io.Writer = (*T)(nil)\n".into(),
        ),
        (
            "A.java",
            format!("class A {{\n    Runnable r = new Runnable() {{\n        public void run() {{}}\n    }};\n    Comparator<X> c = (a, b) -> {{\n        return 0;\n    }};\n    Map<String, List<Integer>> m = new HashMap<>();\n    void z() {{\n{pad}    }}\n}}\n"),
        ),
        (
            "a.kt",
            "val s = $$\"\"\"{ \"$schema\": \"${'$'}x\" }\"\"\"\nfun after() = 1\n".into(),
        ),
        (
            "build.gradle.kts",
            "plugins {\n    kotlin(\"jvm\") version \"1.9\"\n}\ngroup = \"a\"\nversion = \"1\"\ntasks.test {\n    useJUnitPlatform()\n}\n".into(),
        ),
        (
            "E.java",
            format!("enum Op {{\n    PLUS {{ int apply(int a) {{ return a; }} }},\n    MINUS {{ int apply(int a) {{ return -a; }} }};\n    abstract int apply(int a);\n    void z() {{\n{pad}    }}\n}}\n"),
        ),
        (
            "a.go",
            format!("type S struct {{\n{}}}\n", "\tName string `json:\"name\"`\n".repeat(20)),
        ),
        ("a.kt", "fun a() = \"${\"\nfun b() = 1\nfun c() = 2\n".into()),
        (
            "a.kt",
            format!("class A {{\n    val x = this@A.y\n    val z = 2\n    fun q() {{\n{kpad}    }}\n}}\n"),
        ),
        (
            "A.java",
            "class A {\n    private static final Map<String, List<Integer>> M = new HashMap<>();\n}\n".into(),
        ),
        ("a.kt", "fun f(): Int =\n    1 +\n        2\nfun g() = 3\n".into()),
        ("a.kt", "class A<T>(x: T)\n    where T : Comparable<T> {\n}\n".into()),
        ("a.go", "func F(\n\ta int,\n) (int, error) {\n\treturn 0, nil\n}\n".into()),
        ("a.kt", "@Suppress(names = [\"a\"])\nfun f() {}\n".into()),
        (
            "a.kt",
            "sealed class R {\n  data class Ok(val v: Int) : R()\n  object Err : R()\n}\n".into(),
        ),
        (
            "a.py",
            format!("# The widget.\n@dataclass\nclass Widget(Base):\n    \"\"\"Docs.\"\"\"\n\n    def render(self):\n{}", "        value = 1\n".repeat(20)),
        ),
        (
            "package.json",
            format!(
                "{{\n  \"name\": \"demo\",\n  \"dependencies\": {{\n{}    \"zod\": \"3.0.0\"\n  }}\n}}\n",
                "    \"some-long-package-name\": \"1.0.0\",\n".repeat(4)
            ),
        ),
        (
            "a.rs",
            format!("/// Docs.\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    fn a() {{\n{}    }}\n}}\n", "        let x = 1; // padding padding padding\n".repeat(6)),
        ),
    ];
    for (path, src) in &cases {
        assert_nothing_is_dropped(path, src);
    }
}
