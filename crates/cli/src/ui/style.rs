//! Terminal styling and layout: one palette, columns that line up, and file
//! lists that name their directory once.
//!
//! INTENT: one place decides whether escape codes are written — never into a
//! pipe, never under `NO_COLOR` — so a renderer styles freely and a test
//! renders plain text by asking for [`Paint::plain`].
//!
//! The palette is shared with `--help`: [`HELP`] is built from the same
//! constants [`Paint`] writes, so an answer and the help that explains it look
//! like one program. Help text is the one place that writes escapes
//! unconditionally ([`styled`]) — clap strips them itself, under the same two
//! rules, before anything reaches the terminal.

use std::io::IsTerminal;

use clap::builder::styling::{AnsiColor, Style as Ansi, Styles};

/// Emphasis: names, headings.
pub(crate) const BOLD: Ansi = Ansi::new().bold();
/// Secondary information: paths, hints, comments.
pub(crate) const DIM: Ansi = Ansi::new().dimmed();
/// Something that worked, or a value to type.
pub(crate) const GREEN: Ansi = AnsiColor::Green.on_default();
/// Something that failed.
pub(crate) const RED: Ansi = AnsiColor::Red.on_default();
/// A literal to type as written: a command, a flag.
pub(crate) const CYAN: Ansi = AnsiColor::Cyan.on_default();

/// The help palette: bold headings, literals and placeholders coloured, the
/// secondary text dim.
pub(crate) const HELP: Styles = Styles::styled()
    .header(BOLD)
    .usage(BOLD)
    .literal(CYAN.bold())
    .placeholder(GREEN)
    .valid(GREEN)
    .invalid(RED.bold())
    .error(RED.bold())
    .context(DIM);

/// `text` wrapped in `style`, always — for help text, which clap strips of
/// escapes when colour is off.
pub(crate) fn styled(style: Ansi, text: &str) -> String {
    format!("{}{text}{}", style.render(), style.render_reset())
}

/// Whether escape codes are written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Paint {
    color: bool,
}

impl Paint {
    /// No escape codes at all: tests, pipes, `NO_COLOR`.
    pub(crate) fn plain() -> Self {
        Self { color: false }
    }

    /// Colour when stdout is a terminal and `NO_COLOR` is unset.
    pub(crate) fn for_stdout() -> Self {
        if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
            return Self::plain();
        }
        Self { color: true }
    }

    /// Colour when stderr is a terminal and `NO_COLOR` is unset — for what
    /// is printed beside the answer rather than in it.
    pub(crate) fn for_stderr() -> Self {
        let wanted = std::env::var_os("NO_COLOR").is_none();
        Self {
            color: wanted && std::io::stderr().is_terminal(),
        }
    }

    pub(crate) fn bold(self, text: &str) -> String {
        self.wrap(BOLD, text)
    }

    /// Secondary information: paths, hints, headers.
    pub(crate) fn dim(self, text: &str) -> String {
        self.wrap(DIM, text)
    }

    pub(crate) fn green(self, text: &str) -> String {
        self.wrap(GREEN, text)
    }

    pub(crate) fn red(self, text: &str) -> String {
        self.wrap(RED, text)
    }

    pub(crate) fn apply(self, style: Style, text: &str) -> String {
        match style {
            Style::Plain => text.to_string(),
            Style::Bold => self.bold(text),
            Style::Dim => self.dim(text),
        }
    }

    fn wrap(self, style: Ansi, text: &str) -> String {
        if self.color && !text.is_empty() {
            styled(style, text)
        } else {
            text.to_string()
        }
    }
}

/// How one column is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Plain,
    Bold,
    Dim,
}

/// One column's alignment and style.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Col {
    pub(crate) style: Style,
    pub(crate) right: bool,
}

impl Col {
    pub(crate) const LEFT: Col = Col {
        style: Style::Plain,
        right: false,
    };
    pub(crate) const RIGHT: Col = Col {
        style: Style::Plain,
        right: true,
    };
    pub(crate) const BOLD: Col = Col {
        style: Style::Bold,
        right: false,
    };
    pub(crate) const DIM: Col = Col {
        style: Style::Dim,
        right: false,
    };
}

/// Column widths fitted to a set of rows, so rows printed apart — between
/// group headings — still line up.
pub(crate) struct Columns {
    widths: Vec<usize>,
}

impl Columns {
    pub(crate) fn fit<R: AsRef<[String]>>(rows: &[R]) -> Self {
        let mut widths = Vec::new();
        for row in rows {
            for (i, cell) in row.as_ref().iter().enumerate() {
                let w = cell.chars().count();
                match widths.get_mut(i) {
                    Some(width) if *width < w => *width = w,
                    Some(_) => {}
                    None => widths.push(w),
                }
            }
        }
        Self { widths }
    }

    /// One row, two spaces between cells. Padding is computed on the plain
    /// text, then styled, so escape codes never skew a column. The last cell
    /// of a left-aligned row is not padded.
    pub(crate) fn line(&self, row: &[String], cols: &[Col], paint: Paint) -> String {
        let last = row.len().saturating_sub(1);
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let col = cols.get(i).copied().unwrap_or(Col::LEFT);
                let width = self.widths.get(i).copied().unwrap_or(0);
                let padded = match (col.right, i == last) {
                    (true, _) => format!("{cell:>width$}"),
                    (false, false) => format!("{cell:<width$}"),
                    (false, true) => cell.clone(),
                };
                paint.apply(col.style, &padded)
            })
            .collect();
        cells.join("  ").trim_end().to_string()
    }
}

/// Groups file paths under their directory, in first-seen order, so a list of
/// sibling files names the directory once.
pub(crate) fn by_directory(paths: &[String]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for path in paths {
        let path = path.replace('\\', "/");
        let (dir, file) = match path.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), file.to_string()),
            None => (String::new(), path.clone()),
        };
        match groups.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, files)) => files.push(file),
            None => groups.push((dir, vec![file])),
        }
    }
    groups
}

/// `1 project`, `3 projects`.
pub(crate) fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `6 888`, `1 204 551`: digits grouped by three, as a count is read.
pub(crate) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

/// A size in bytes, as a person reads it.
pub(crate) fn size(bytes: u64) -> String {
    const MB: f64 = 1_000_000.0;
    match bytes {
        0 => "0 B".to_string(),
        b if b < 100_000 => format!("{:.1} kB", b as f64 / 1_000.0),
        b => format!("{:.1} MB", b as f64 / MB),
    }
}

/// `` `a`, `b` ``.
pub(crate) fn quoted(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn columns_line_up_across_rows_printed_apart() {
        let rows = [row(&["a", "12", "x"]), row(&["long", "3", "yy"])];
        let fit = Columns::fit(&rows);
        let cols = [Col::LEFT, Col::RIGHT, Col::LEFT];
        assert_eq!(fit.line(&rows[0], &cols, Paint::plain()), "a     12  x");
        assert_eq!(fit.line(&rows[1], &cols, Paint::plain()), "long   3  yy");
    }

    #[test]
    fn plain_paint_writes_no_escape_codes_and_colour_wraps_after_padding() {
        let fit = Columns::fit(&[row(&["ab", "c"])]);
        let plain = fit.line(&row(&["a", "c"]), &[Col::BOLD, Col::LEFT], Paint::plain());
        assert_eq!(plain, "a   c");
        let color = Paint { color: true };
        let styled = fit.line(&row(&["a", "c"]), &[Col::BOLD, Col::LEFT], color);
        assert_eq!(
            styled, "\x1b[1ma \x1b[0m  c",
            "padding sits inside the style"
        );
    }

    #[test]
    fn files_in_one_directory_name_it_once() {
        let groups = by_directory(&[
            "C:/p/.findery/bucket.a.db".into(),
            "C:\\p\\.findery\\bucket.b.db".into(),
            "D:/x.db".into(),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "C:/p/.findery/");
        assert_eq!(groups[0].1, vec!["bucket.a.db", "bucket.b.db"]);
        assert_eq!(groups[1], ("D:/".into(), vec!["x.db".into()]));
    }

    #[test]
    fn counts_and_sizes_read_like_prose() {
        assert_eq!(count(1, "project", "projects"), "1 project");
        assert_eq!(count(0, "project", "projects"), "0 projects");
        assert_eq!(size(0), "0 B");
        assert_eq!(size(4_096), "4.1 kB");
        assert_eq!(size(138_412_032), "138.4 MB");
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(312), "312");
        assert_eq!(grouped(6_888), "6 888");
        assert_eq!(grouped(1_204_551), "1 204 551");
    }
}
