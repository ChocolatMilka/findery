//! The literals a `${…}` template may sit in: Kotlin's strings and raw
//! strings, Java's text blocks, and the strings that end at their line.
//!
//! INTENT: a template is code inside a literal, so reading one is the only
//! recursion of the scanner — and the only place a half-typed file can make a
//! scan explode. Holding it apart keeps the bound on that recursion, and the
//! fallback when a template never closes, in one place.

use super::{Lexicon, TextBlocks, construct_end};

/// How deep `${…}` templates may nest inside each other before the scanner
/// stops re-entering them and reads the rest of the template flat.
///
/// INTENT: following a template is recursion, and recursion on input is a
/// stack the input controls. Real code nests two or three levels; this bounds
/// the stack, and [`Unclosed`] bounds the time.
const MAX_TEMPLATE_DEPTH: usize = 16;

/// How many lines a template's open bracket may carry it over.
///
/// INTENT: an argument list spread over lines inside a template is real
/// Kotlin; a template that never closes must still stop somewhere close, or
/// every line of a file of `"${(` would scan to its end.
const MAX_TEMPLATE_LINES: usize = 16;

/// A `${…}` template that does not close on its own line: a half-typed
/// `"${x` in a file being edited.
///
/// INTENT: the template does not guess where it ends. It reports up to the
/// outermost literal, which reads itself again with its templates as plain
/// text — so `println("${x")` ends at its quote and its `)` still closes the
/// call. Each literal is read at most twice, where letting every enclosing
/// template rescan the rest of the file made thirty `"${` lines take longer
/// than a whole sync.
pub(super) struct Unclosed;

/// Where a literal ends, or that a template inside it never closed.
pub(super) type End = Result<usize, Unclosed>;

/// How a literal reads its `${…}` templates.
#[derive(Clone, Copy)]
pub(super) enum Templates {
    /// As code, this many templates deep already.
    Followed(usize),
    /// As plain text of the literal.
    Flat,
}

/// The literal opening at `i`, if it is one a template may sit in: a text
/// block, or a string of a language that follows templates or ends a string
/// at the newline.
pub(super) fn literal_end(
    src: &[u8],
    i: usize,
    lex: &Lexicon,
    templates: Templates,
) -> Option<End> {
    let b = *src.get(i)?;
    if lex.text_blocks != TextBlocks::No && src[i..].starts_with(b"\"\"\"") {
        return Some(skip_text_block(src, i + 3, lex, templates));
    }
    if lex.strings.contains(&b) {
        return Some(skip_line_string(src, i + 1, b, lex, templates));
    }
    None
}

/// Skips the `${` at `i` and the template it opens, as `templates` says.
fn skip_template_at(src: &[u8], i: usize, lex: &Lexicon, templates: Templates) -> End {
    match templates {
        Templates::Followed(depth) => skip_template(src, i + 2, lex, depth),
        Templates::Flat => Ok(i + 2),
    }
}

/// Skips a string of a language that may follow templates and may forbid a
/// newline inside it.
///
/// INTENT: a string that cannot span a line ends at the newline when its quote
/// never comes — the compiler would refuse it there, and the scanner refusing
/// to let it swallow the file keeps every item below it. The newline itself is
/// left to the caller, which may need it to end a statement.
fn skip_line_string(
    src: &[u8],
    mut i: usize,
    quote: u8,
    lex: &Lexicon,
    templates: Templates,
) -> End {
    let has_templates = lex.templates && quote == b'"';
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\n' if lex.single_line_strings => return Ok(i),
            b'$' if has_templates && src.get(i + 1) == Some(&b'{') => {
                i = skip_template_at(src, i, lex, templates)?;
            }
            b if b == quote => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Ok(i.min(src.len()))
}

/// Skips the code of a `${…}` template up to its matching `}`.
///
/// INTENT: the expression is code, so it may hold a string that holds a brace
/// — `"${m["{"]}"` — and only re-entering the scanner tells that brace from
/// the one that closes the template. It runs over a line only inside a bracket
/// it opened — `"${minOf(` / `size,` / `)}"` — and over [`MAX_TEMPLATE_LINES`]
/// at most: anywhere else, a newline before its `}` means it never closed.
fn skip_template(src: &[u8], start: usize, lex: &Lexicon, depth: usize) -> End {
    let mut i = start;
    let mut braces = 1usize;
    let mut brackets = 0usize;
    let mut lines = 0usize;
    while i < src.len() {
        if depth < MAX_TEMPLATE_DEPTH
            && let Some(end) = construct_end(src, i, lex, depth + 1)
        {
            i = end?.max(i + 1);
            continue;
        }
        match src[i] {
            b'\n' if braces == 1 && brackets == 0 => return Err(Unclosed),
            b'\n' if lines == MAX_TEMPLATE_LINES => return Err(Unclosed),
            b'\n' => lines += 1,
            b'(' | b'[' => brackets += 1,
            b')' | b']' => brackets = brackets.saturating_sub(1),
            b'{' => braces += 1,
            b'}' => {
                braces -= 1;
                if braces == 0 {
                    return Ok(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err(Unclosed)
}

/// Skips a `"""…"""` literal opened just before `i`. The closing run may be
/// longer than three quotes — `""""` ends a raw string on a quote — and every
/// quote of it belongs to the literal.
fn skip_text_block(src: &[u8], mut i: usize, lex: &Lexicon, templates: Templates) -> End {
    while i < src.len() {
        if src[i] == b'\\' && lex.text_blocks == TextBlocks::Escaped {
            i += 2;
            continue;
        }
        if lex.templates && src[i] == b'$' && src.get(i + 1) == Some(&b'{') {
            i = skip_template_at(src, i, lex, templates)?;
            continue;
        }
        if src[i..].starts_with(b"\"\"\"") {
            i += 3;
            while src.get(i) == Some(&b'"') {
                i += 1;
            }
            return Ok(i);
        }
        i += 1;
    }
    Ok(i.min(src.len()))
}
