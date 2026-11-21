//! The chunking invariants, over every language: no content is lost, no chunk
//! outgrows its budget, and a name is carried by the chunk that bears it.
//!
//! INTENT: a chunker that splits wrongly usually fails LOUDLY — a wrong name, a
//! wrong kind, a test asserting on it. A chunker that DROPS text fails in
//! silence: the file is indexed, every chunk looks sane, and a function nobody
//! will ever find again is simply not there. These invariants make that
//! failure loud, for every language at once: every word of every source line
//! is carried, as a whole word, by a chunk whose line range covers that line.

mod fixtures;

use findery_chunk::Registry;
use findery_core::config::ChunkConfig;
use findery_core::model::{ChunkKind, Lang, RawChunk};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A budget that forces every container to be split, and the default one.
const BUDGETS: &[usize] = &[120, 2400];

/// How far a chunk may outgrow `max_chars`: a budget narrower than one UTF-8
/// character still takes that character whole.
const SIZE_MARGIN: usize = 3;

/// Lines a chunker drops on purpose, as `.claude/docs/chunk/README.md` says it
/// does: a JavaScript shebang and a PHP opening tag say nothing.
fn dropped_on_purpose(path: &str, line_no: usize, line: &str) -> bool {
    let shebang =
        line_no == 1 && line.starts_with("#!") && Lang::from_path(path) == Lang::JavaScript;
    shebang || line.trim() == "<?php"
}

/// The words of a line: what a search could look for in it.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

fn chunk(path: &str, src: &str, max_chars: usize) -> Vec<RawChunk> {
    let cfg = ChunkConfig {
        max_chars,
        include_tests: true,
        ..ChunkConfig::default()
    };
    Registry::new()
        .chunk(path, Lang::from_path(path), src, &cfg)
        .expect("chunking succeeds")
}

/// Fails when a chunk of `src` breaks an invariant, at every budget.
fn assert_invariants(path: &str, src: &str) {
    for &max_chars in BUDGETS {
        let out = chunk(path, src, max_chars);
        assert_nothing_is_dropped(path, src, max_chars, &out);
        assert_within_budget(path, max_chars, &out);
        assert_symbols_are_carried(path, max_chars, &out);
    }
}

/// Every word of every line is carried, whole, by a chunk covering its line.
fn assert_nothing_is_dropped(path: &str, src: &str, max_chars: usize, out: &[RawChunk]) {
    let lines: Vec<&str> = src.lines().collect();
    let carried: Vec<HashSet<&str>> = out.iter().map(|c| words(&c.text).collect()).collect();
    let mut covering: Vec<Vec<usize>> = vec![Vec::new(); lines.len() + 1];
    for (k, c) in out.iter().enumerate() {
        let last = (c.end_line as usize).min(lines.len());
        for chunks in &mut covering[c.start_line as usize..=last] {
            chunks.push(k);
        }
    }
    for (k, line) in lines.iter().enumerate() {
        let line_no = k + 1;
        if dropped_on_purpose(path, line_no, line) {
            continue;
        }
        for word in words(line) {
            let found = covering[line_no].iter().any(|&c| carried[c].contains(word));
            assert!(
                found,
                "{path} at max_chars = {max_chars}: line {line_no} `{line}` lost `{word}`\n{out:#?}"
            );
        }
    }
}

/// No chunk outgrows the budget.
fn assert_within_budget(path: &str, max_chars: usize, out: &[RawChunk]) {
    for c in out {
        assert!(
            c.text.len() <= max_chars + SIZE_MARGIN,
            "{path} at max_chars = {max_chars}: a chunk of {} bytes\n{c:#?}",
            c.text.len()
        );
    }
}

/// The name an item bears is in its text: a symbol pointing at chunks that do
/// not hold its declaration is a split in the wrong place. The windows of one
/// item all bear its name, so they are read together. A name taken from the
/// path (a module) or given by the language (`Companion`) is in no text.
fn assert_symbols_are_carried(path: &str, max_chars: usize, out: &[RawChunk]) {
    for item in out
        .chunk_by(|a, b| (a.kind, &a.symbol, &a.breadcrumb) == (b.kind, &b.symbol, &b.breadcrumb))
    {
        let Some(symbol) = &item[0].symbol else {
            continue;
        };
        let from_path = item[0].kind == ChunkKind::Module
            && item[0].breadcrumb.as_deref() == Some(&format!("module {symbol}"));
        if from_path || symbol == "Companion" {
            continue;
        }
        let text: HashSet<&str> = item.iter().flat_map(|c| words(&c.text)).collect();
        for word in words(symbol) {
            assert!(
                text.contains(word),
                "{path} at max_chars = {max_chars}: `{symbol}` is not in its chunks\n{item:#?}"
            );
        }
    }
}

/// Fails unless a chunk bearing `symbol` covers `line` once every container
/// is split: a declaration's line read into the wrong item is caught here.
fn assert_declared(path: &str, src: &str, declarations: &[(usize, &str)]) {
    let max_chars = BUDGETS[0];
    let out = chunk(path, src, max_chars);
    for &(line, symbol) in declarations {
        let found = out.iter().any(|c| {
            c.symbol.as_deref() == Some(symbol)
                && (c.start_line as usize..=c.end_line as usize).contains(&line)
        });
        assert!(
            found,
            "{path} at max_chars = {max_chars}: no chunk named `{symbol}` covers line {line}\n{out:#?}"
        );
    }
}

/// The line of `src` that holds `needle`.
fn line_of(src: &str, needle: &str) -> usize {
    1 + src
        .lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in {src}"))
}

#[test]
fn every_fixture_keeps_every_word_of_every_line() {
    for (path, src) in fixtures::ALL {
        assert_invariants(path, src);
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
        assert_invariants(path, src);
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
            "a.kt",
            format!("class Gate(val ok: Boolean = x < 1) {{\n{funs}}}\n"),
        ),
        (
            "a.ts",
            format!("@Component({{\n  selector: 'x'\n}})\nexport class W {{\n{members}}}\n"),
        ),
    ];
    for (path, src) in &cases {
        assert_invariants(path, src);
        assert_declared(path, src, &[(line_of(src, " b()"), "b")]);
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
        (
            "a.rs",
            format!("#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    fn a() {{\n{}    }}\n}}\n", "        let x = 1; // padding padding padding\n".repeat(6)),
        ),
        ("a.rs", "fn f(x: u8) {\n    match x {\n        _ => {}\n    }\n}\n".into()),
    ];
    for (path, src) in &cases {
        assert_invariants(path, src);
    }
}

/// The line a declaration sits on, and the symbol its chunk must bear.
type Declared = Vec<(usize, &'static str)>;

/// The shapes real repositories hold that a chunker once misread, each with
/// the declarations it must still name.
#[test]
fn the_real_world_shapes_keep_their_words_and_their_names() {
    let kbody = format!(
        "{{\n    fun x() = 1\n{}}}\n",
        "    fun pad() = println(\"padding padding padding padding\")\n".repeat(3)
    );
    let cases: Vec<(&str, String, Declared)> = vec![
        (
            "a.kt",
            format!("class A\n    internal actual constructor(val b: B) {kbody}"),
            vec![(2, "A")],
        ),
        (
            "a.kt",
            format!("class A\n/** KDoc on the constructor. */\n@Inject constructor(val b: B) {kbody}"),
            vec![(3, "A")],
        ),
        (
            "a.kt",
            format!("class A\n    @Inject\n    constructor(val b: B) {kbody}"),
            vec![(3, "A")],
        ),
        (
            "a.kt",
            format!("class A private\nconstructor(val b: B) {kbody}"),
            vec![(2, "A")],
        ),
        (
            "a.kt",
            format!("class `My Class` {kbody}\ninterface `-Iface` {{\n    fun a()\n}}\n"),
            vec![(1, "My Class")],
        ),
        (
            "a.kt",
            "val Int.isSurrogate: Boolean\n    inline get() = this > 1\nval handle: Long\n    external get\n".into(),
            vec![(2, "Int.isSurrogate"), (4, "handle")],
        ),
        (
            "tsconfig.json",
            "{\n  \"extends\": \"./base.json\",\n  \"compilerOptions\": {\n    \"allowJs\": true,\n    \"checkJs\": false,\n    \"moduleResolution\": \"bundler\"\n  }\n  // Path aliases are handled by the kit configuration.\n  //\n  // If you want to overwrite includes, copy them over.\n}\n".into(),
            vec![(3, "compilerOptions")],
        ),
        (
            "a.json",
            "{\n  \"a\": \"first value of the document\",\n  // between\n  \"b\": \"second value of the document\",\n  \"c\": \"third value of the document\"\n}\n".into(),
            vec![(4, "b")],
        ),
        ("a.py", "# Licence: MIT\n# Nothing but comments.\n".into(), vec![]),
        ("a.py", "#!/usr/bin/env python3".into(), vec![]),
        (
            "a.py",
            format!("@dataclass(\n    frozen=True,\n)\nclass Widget:\n    x = 1\n{}", "    y = 2 # padding padding padding padding\n".repeat(4)),
            vec![(4, "Widget")],
        ),
        (
            "a.py",
            format!("class T:\n    @pytest.mark.parametrize(\n        \"x\", [1, 2],\n    )\n    def test_a(self, x):\n{}", "        assert x # padding padding padding\n".repeat(4)),
            vec![(5, "test_a")],
        ),
        (
            "src/lib.rs",
            format!("{}\npub fn a() {{}}\n", "//! A line of the crate's documentation, long enough.\n".repeat(12)),
            vec![(14, "a")],
        ),
        (
            "app.min.js",
            format!("{}\n", "var a=function(b){return b+1};".repeat(200)),
            vec![],
        ),
        (
            "Point.java",
            "public record Point(int x, int y) {\n    public Point {\n        if (x < 0) throw new IllegalArgumentException(\"x\");\n    }\n\n    public int sum() { return x + y; }\n}\n".into(),
            vec![(1, "Point")],
        ),
        (
            "shapes.go",
            "type (\n\tA struct{ x int }\n\tB interface{ M() }\n)\n".into(),
            vec![(2, "A"), (3, "B")],
        ),
        (
            "get.go",
            "func Get(key string) interface{} {\n\treturn nil\n}\n\nfunc after() {}\n".into(),
            vec![(1, "Get"), (3, "Get")],
        ),
        (
            "a.kt",
            "fun a() {\n    println(\"${x\")\n}\n\nfun b() {}\n\nfun c() {}\n".into(),
            vec![(5, "b"), (7, "c")],
        ),
    ];
    for (path, src, declarations) in &cases {
        assert_invariants(path, src);
        assert_declared(path, src, declarations);
    }
}

/// How deep the pesticide nests: far past any recursion a 256 KiB stack
/// survives.
const DEEP: usize = 20_000;

/// One file of `DEEP` nested containers, per language that nests them.
fn deeply_nested(path: &str) -> String {
    let body = "int x = 1; // padding padding padding padding padding\n";
    let (open, close) = match Lang::from_path(path) {
        Lang::Rust => ("mod a {\n", "}\n"),
        Lang::Go => ("type A interface {\n", "}\n"),
        Lang::Json => ("{\"a\": ", "}"),
        _ => ("class A {\n", "}\n"),
    };
    match Lang::from_path(path) {
        Lang::Python => (0..DEEP)
            .map(|d| format!("{}class A:\n", " ".repeat(d.min(256))))
            .collect(),
        _ => format!("{}{body}{}", open.repeat(DEEP), close.repeat(DEEP)),
    }
}

#[test]
fn deeply_nested_containers_cost_a_bounded_stack_and_time() {
    // Chunking runs inline in the daemon's sync: a stack overflow there aborts
    // the process, and catch_unwind cannot catch it.
    let paths = [
        "a.rs", "a.js", "a.ts", "a.php", "a.axle", "A.java", "a.kt", "a.go", "a.py", "a.json",
    ];
    for path in paths {
        let src = deeply_nested(path);
        let worker = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || {
                let started = Instant::now();
                assert_invariants(path, &src);
                started.elapsed()
            })
            .expect("spawn a worker");
        let elapsed = worker.join().expect("the worker survives");
        assert!(
            elapsed < Duration::from_secs(20),
            "{path}: {DEEP} nested containers took {elapsed:?}"
        );
    }
}
