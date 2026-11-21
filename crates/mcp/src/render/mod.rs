//! How every answer reads: laid out once for a model to read and cite.
//!
//! INTENT: the rendering IS the product here. Raw JSON spends a third of its
//! tokens on punctuation and field names the model has no use for; these
//! layouts carry a citable locator, the excerpt, and the one line of advice
//! that saves the next call — nothing else.

pub mod jobs;
pub mod projects;
pub mod read;
pub mod search;

use findery_core::model::Lang;
use findery_core::support::clock::now_unix;

/// What the daemon appends to an excerpt it had to cut.
const DAEMON_CUT_MARK: &str = "\n… [truncated]";

/// `note: …` lines, one per warning, for the top of an answer.
pub fn notes(warnings: &[String]) -> String {
    warnings.iter().map(|w| format!("note: {w}\n")).collect()
}

/// A fenced block of `text`, tagged for `path`. A cut excerpt says where the
/// rest is.
pub fn fenced(path: &str, lang: Lang, text: &str, locator: &str) -> String {
    let (body, cut) = match text.strip_suffix(DAEMON_CUT_MARK) {
        Some(body) => (body, true),
        None => (text, false),
    };
    let mut out = format!("```{}\n{}\n```\n", fence_tag(path, lang), body.trim_end());
    if cut {
        out.push_str(&format!("(cut — read {locator} for the rest)\n"));
    }
    out
}

/// The tag a Markdown fence takes for this file.
pub fn fence_tag(path: &str, lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "rust",
        Lang::Markdown => "markdown",
        Lang::Toml => "toml",
        Lang::JavaScript if is_typescript(path) => "typescript",
        Lang::JavaScript => "javascript",
        Lang::Python => "python",
        Lang::Php => "php",
        Lang::Json => "json",
        Lang::Axle => "axle",
        Lang::Java => "java",
        Lang::Kotlin => "kotlin",
        Lang::Go => "go",
        Lang::Other => "text",
    }
}

fn is_typescript(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or_default();
    matches!(ext, "ts" | "mts" | "cts")
}

/// A language as a person names it.
pub fn lang_name(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "Rust",
        Lang::Markdown => "Markdown",
        Lang::Toml => "TOML",
        Lang::JavaScript => "JavaScript/TypeScript",
        Lang::Python => "Python",
        Lang::Php => "PHP",
        Lang::Json => "JSON",
        Lang::Axle => "Axle",
        Lang::Java => "Java",
        Lang::Kotlin => "Kotlin",
        Lang::Go => "Go",
        Lang::Other => "other",
    }
}

/// `42 s ago`, `3 h ago`, `2 d ago` — or `never`.
pub fn ago(unix: Option<i64>) -> String {
    let Some(then) = unix else {
        return "never synced".to_string();
    };
    let secs = (now_unix() - then).max(0);
    let (n, unit) = match secs {
        0..60 => (secs, "s"),
        60..3_600 => (secs / 60, "min"),
        3_600..86_400 => (secs / 3_600, "h"),
        _ => (secs / 86_400, "d"),
    };
    format!("synced {n} {unit} ago")
}

/// `text` cut to `max` characters on a char boundary, with an ellipsis.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// `a, b and c`.
pub fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_excerpt_points_at_the_rest_and_a_whole_one_does_not() {
        let cut = fenced(
            "src/a.rs",
            Lang::Rust,
            "fn a() {\n… [truncated]",
            "src/a.rs:L1-L9",
        );
        assert!(cut.starts_with("```rust\nfn a() {\n```"), "{cut}");
        assert!(cut.contains("read src/a.rs:L1-L9 for the rest"), "{cut}");
        let whole = fenced(
            "web/app.ts",
            Lang::JavaScript,
            "let a = 1;\n",
            "web/app.ts:L1-L1",
        );
        assert_eq!(whole, "```typescript\nlet a = 1;\n```\n");
    }

    #[test]
    fn long_text_is_clipped_on_a_character_and_says_so() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("élan vital", 5), "élan…");
        assert_eq!(
            join_and(&["a".into(), "b".into(), "c".into()]),
            "a, b and c"
        );
    }

    #[test]
    fn freshness_reads_in_the_largest_unit_and_never_is_said() {
        assert_eq!(ago(None), "never synced");
        assert_eq!(ago(Some(now_unix() - 7_200)), "synced 2 h ago");
    }
}
