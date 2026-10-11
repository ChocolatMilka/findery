//! A minimal lexical scanner: what a language's punctuation can be trusted for.
//!
//! INTENT: chunking needs item boundaries, not a syntax tree. A full parser
//! (tree-sitter, syn) would drag a C toolchain or a proc-macro dependency into
//! the image for information we throw away immediately. What we do need is to
//! never mistake a brace inside a string, a comment or a raw literal for a
//! structural brace — that is exactly what this scanner guarantees.

mod lexicon;

pub use lexicon::{BlockComment, Lexicon};

/// Skips a lexical construct whose contents must not be read structurally.
///
/// Returns the offset just past the construct, or `None` when the byte at `i`
/// starts no such construct and the caller should interpret it.
pub fn skip_opaque(src: &[u8], i: usize, lex: &Lexicon) -> Option<usize> {
    let b = *src.get(i)?;
    for open in lex.line_comments {
        if src[i..].starts_with(open.as_bytes()) {
            // `#[…]` is an attribute, not a `#` line comment (PHP 8, Rust).
            if *open == "#" && src.get(i + 1) == Some(&b'[') {
                continue;
            }
            return Some(skip_line_comment(src, i + open.len()));
        }
    }
    if let Some(bc) = &lex.block_comment
        && src[i..].starts_with(bc.open.as_bytes())
    {
        return Some(skip_block_comment(src, i + bc.open.len(), bc));
    }
    if lex.raw_strings
        && (b == b'r' || b == b'b')
        && let Some(end) = skip_raw_or_byte_literal(src, i)
    {
        return Some(end);
    }
    if lex.char_literals && b == b'\'' {
        return skip_char_literal(src, i);
    }
    if lex.heredocs && src[i..].starts_with(b"<<<") {
        return skip_heredoc(src, i);
    }
    if lex.regex_literals && b == b'/' && regex_allowed(src, i) {
        return Some(skip_regex(src, i + 1));
    }
    if lex.strings.contains(&b) {
        return Some(skip_string(src, i + 1, b));
    }
    None
}

fn skip_line_comment(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && src[i] != b'\n' {
        i += 1;
    }
    i
}

/// Skips a block comment. Nesting is a property of the language, not of the
/// scanner: Rust nests, C, JavaScript and PHP do not.
fn skip_block_comment(src: &[u8], mut i: usize, bc: &BlockComment) -> usize {
    let (open, close) = (bc.open.as_bytes(), bc.close.as_bytes());
    let mut depth = 1usize;
    while i < src.len() {
        if bc.nested && src[i..].starts_with(open) {
            depth += 1;
            i += open.len();
        } else if src[i..].starts_with(close) {
            depth -= 1;
            i += close.len();
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    i
}

/// Skips an escaped string delimited by `quote`.
///
/// A template literal (`` ` ``) is treated as one string, `${…}` included: a
/// nested template inside an interpolation is the one case this does not
/// follow, and it is rare enough not to buy a state machine for.
fn skip_string(src: &[u8], mut i: usize, quote: u8) -> usize {
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Handles `r"..."`, `r#"..."#`, `b"..."`, `br#"..."#`.
fn skip_raw_or_byte_literal(src: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    if src.get(j) == Some(&b'b') {
        j += 1;
    }
    let raw = src.get(j) == Some(&b'r');
    if raw {
        j += 1;
    }
    if !raw {
        // `b"..."`: a plain escaped string.
        return if src.get(j) == Some(&b'"') && j > i {
            Some(skip_string(src, j + 1, b'"'))
        } else {
            None
        };
    }
    let hashes = {
        let mut n = 0usize;
        while src.get(j + n) == Some(&b'#') {
            n += 1;
        }
        n
    };
    j += hashes;
    if src.get(j) != Some(&b'"') {
        return None; // an identifier such as `result` or `raw_mode`, not a literal
    }
    j += 1;
    // Closing delimiter is `"` followed by exactly `hashes` `#`.
    while j < src.len() {
        if src[j] == b'"' {
            let mut k = j + 1;
            let mut n = 0usize;
            while n < hashes && src.get(k) == Some(&b'#') {
                k += 1;
                n += 1;
            }
            if n == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    Some(j)
}

/// Distinguishes a char literal from a lifetime: `'a'` is a literal, `'a` is a
/// lifetime and must be stepped over one byte at a time.
///
/// INTENT: the width of the character is read from its UTF-8 lead byte, not
/// assumed to be one. `'é'`, `'→'` and `'…'` are 4, 5 and 4 bytes long, and
/// reading the closing quote as the OPENING quote of the next literal swallows
/// it — which turns the brace of the following item into a structural brace and
/// silently fuses two items into one.
fn skip_char_literal(src: &[u8], i: usize) -> Option<usize> {
    let first = *src.get(i + 1)?;
    if first == b'\\' {
        // An escape runs to the next `'`: `'\''`, `'\\'`, `'\u{1F600}'`.
        let mut j = i + 3;
        while j < src.len() && src[j] != b'\'' {
            j += 1;
        }
        return Some(j + 1);
    }
    let width = match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    (src.get(i + 1 + width) == Some(&b'\'')).then_some(i + 2 + width)
}

/// Handles a PHP heredoc/nowdoc: `<<<EOT`, `<<<"EOT"`, `<<<'EOT'`, closed by a
/// line whose first non-blank token is the same label.
fn skip_heredoc(src: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 3;
    let quote = match src.get(j) {
        Some(q @ (b'"' | b'\'')) => {
            j += 1;
            Some(*q)
        }
        _ => None,
    };
    let label_start = j;
    while src
        .get(j)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    {
        j += 1;
    }
    if j == label_start {
        return None; // `<<<` with nothing usable: not a heredoc
    }
    let label = src[label_start..j].to_vec();
    if let Some(q) = quote {
        if src.get(j) != Some(&q) {
            return None;
        }
        j += 1;
    }
    // A heredoc's label must be followed by the newline that opens it.
    if src.get(j) != Some(&b'\n') {
        return None;
    }
    let mut line = j + 1;
    loop {
        let mut k = line;
        while src.get(k).is_some_and(|c| *c == b' ' || *c == b'\t') {
            k += 1;
        }
        if src[k..].starts_with(&label) {
            let after = k + label.len();
            let is_word = src
                .get(after)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
            if !is_word {
                return Some(after);
            }
        }
        while line < src.len() && src[line] != b'\n' {
            line += 1;
        }
        if line >= src.len() {
            return Some(src.len());
        }
        line += 1;
    }
}

/// Whether a `/` sitting at `i` can start a regular expression rather than a
/// division. The preceding token answers it: after an identifier, a number, a
/// `)` or a `]`, it is a division; after an operator or one of the keywords
/// that take an expression, it is a regex.
fn regex_allowed(src: &[u8], i: usize) -> bool {
    const EXPRESSION_KEYWORDS: &[&[u8]] = &[
        b"return",
        b"typeof",
        b"instanceof",
        b"in",
        b"of",
        b"new",
        b"delete",
        b"void",
        b"throw",
        b"case",
        b"do",
        b"else",
        b"yield",
        b"await",
    ];
    let mut j = i;
    while j > 0 && src[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if j == 0 {
        return true;
    }
    let mut k = j;
    while k > 0 && (src[k - 1].is_ascii_alphanumeric() || src[k - 1] == b'_') {
        k -= 1;
    }
    if k < j && EXPRESSION_KEYWORDS.contains(&&src[k..j]) {
        return true;
    }
    !matches!(
        src[j - 1],
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b')' | b']' | b'}' | b'"' | b'\'' | b'`'
    )
}

/// Skips `/…/flags`. Stops at the newline: an unterminated regex is a
/// misread division, and swallowing the rest of the file for it would be worse
/// than the misreading.
fn skip_regex(src: &[u8], mut i: usize) -> usize {
    let mut in_class = false;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'[' => {
                in_class = true;
                i += 1;
            }
            b']' => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => {
                i += 1;
                while src.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
                    i += 1;
                }
                return i;
            }
            b'\n' => return i,
            _ => i += 1,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &Lexicon = &Lexicon::RUST;
    const JS: &Lexicon = &Lexicon::JAVASCRIPT;
    const PHP: &Lexicon = &Lexicon::PHP;

    /// The offset just past the first opaque construct found at or after `from`.
    fn opaque(src: &str, from: usize, lex: &Lexicon) -> Option<usize> {
        skip_opaque(src.as_bytes(), from, lex)
    }

    #[test]
    fn a_character_literal_costs_its_own_width() {
        // `'é'` is four bytes: byte-arithmetic reads its closing quote as the
        // opening quote of the next literal and loses the brace that follows.
        assert_eq!(opaque("'é'", 0, RUST), Some(4));
        assert_eq!(opaque("'→'", 0, RUST), Some(5));
    }

    #[test]
    fn an_escaped_quote_ends_the_literal_it_belongs_to() {
        let src = r"'\''";
        assert_eq!(opaque(src, 0, RUST), Some(4));
        let src = r"'\\'";
        assert_eq!(opaque(src, 0, RUST), Some(4));
        let src = r"'\u{1F600}'";
        assert_eq!(opaque(src, 0, RUST), Some(src.len()));
    }

    #[test]
    fn a_lifetime_is_not_a_character_literal() {
        assert_eq!(opaque("'a", 0, RUST), None);
    }

    #[test]
    fn a_regex_literal_is_opaque_only_where_an_expression_can_start() {
        let src = "const re = /a{2}/g;";
        let at = src.find('/').expect("slash");
        assert_eq!(opaque(src, at, JS), Some(at + 7));
        // `a / b` is a division: the operand before it decides.
        let src = "const x = a / b;";
        let at = src.find('/').expect("slash");
        assert_eq!(opaque(src, at, JS), None);
        // `return` takes an expression, so the regex is a regex.
        let src = "return /a{2}/;";
        let at = src.find('/').expect("slash");
        assert_eq!(opaque(src, at, JS), Some(at + 6));
    }

    #[test]
    fn a_hash_attribute_is_not_a_hash_comment() {
        assert_eq!(opaque("#[Attribute]", 0, PHP), None);
        let src = "# a comment";
        assert_eq!(opaque(src, 0, PHP), Some(src.len()));
    }

    #[test]
    fn a_heredoc_is_opaque_until_its_label_comes_back() {
        let src = "$a = <<<EOT\n{ not a brace }\nEOT;\n$x = 1;";
        let end = opaque(src, 5, PHP).expect("heredoc");
        assert_eq!(&src[end..end + 1], ";");
    }

    #[test]
    fn a_block_comment_nests_only_where_the_language_says_it_does() {
        let src = "/* /* */ still inside */";
        assert_eq!(opaque(src, 0, RUST), Some(src.len()));
        let src = "/* /* */ done */";
        assert_eq!(opaque(src, 0, JS), Some(8));
    }
}
