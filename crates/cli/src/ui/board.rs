//! The live view of a sync on stderr: a status line while nothing is planned,
//! then up to two bars — files, and embeddings — each drawn once it means
//! something.
//!
//! ```text
//! files ============================ 312/312 · 6 888 unchanged
//! embed ===============>             1 541/2 300 · 5 937 cached · 38/s · ETA 0:42
//! ```
//!
//! INTENT: the wording is a pure function of a [`Progress`] ([`view`]), so it
//! is tested as text; [`Board`] only hands it to indicatif.

use std::io::IsTerminal;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use findery_core::api::{Progress, ProgressPhase};

use crate::ui::style::{Paint, grouped};

/// One bar: where it is, how long it is (`None`: a count with no
/// denominator), and what it says after.
#[derive(Debug, PartialEq)]
pub(crate) struct Line {
    pub(crate) pos: u64,
    pub(crate) len: Option<u64>,
    pub(crate) text: String,
}

/// What one snapshot shows.
#[derive(Debug, PartialEq)]
pub(crate) enum View {
    /// The walk is running: a spinner and a count.
    Scanning(String),
    /// The plan is known.
    Working { files: Line, embed: Option<Line> },
}

/// The wording of one snapshot.
pub(crate) fn view(p: &Progress, paint: Paint) -> View {
    if p.phase == ProgressPhase::Scanning {
        return View::Scanning(scanning(p.files_seen));
    }
    View::Working {
        files: files_line(p, paint),
        embed: embed_line(p, paint),
    }
}

pub(crate) fn scanning(seen: u64) -> String {
    format!("scanning… {} files", grouped(seen))
}

fn files_line(p: &Progress, paint: Paint) -> Line {
    let mut text = format!("{}/{}", grouped(p.files_done), grouped(p.files_pending));
    if p.files_unchanged > 0 {
        text.push_str(&paint.dim(&format!(" · {} unchanged", grouped(p.files_unchanged))));
    }
    if p.phase == ProgressPhase::Pruning {
        text.push_str(&paint.dim(" · pruning"));
    }
    Line {
        pos: p.files_done,
        len: Some(p.files_pending),
        text,
    }
}

/// Absent until there is something to say: a pass that embeds nothing and
/// finds nothing cached has no embedding line at all.
fn embed_line(p: &Progress, paint: Paint) -> Option<Line> {
    let known = p.chunks_pending_embed;
    if known == 0 && p.embeddings_computed == 0 && p.embeddings_cached == 0 {
        return None;
    }
    let mut text = match known {
        0 => grouped(p.embeddings_computed),
        _ => format!("{}/{}", grouped(p.embeddings_computed), grouped(known)),
    };
    let mut extra = vec![format!("{} cached", grouped(p.embeddings_cached))];
    if p.rate > 0.0 {
        extra.push(format!("{}/s", rate(p.rate)));
    }
    if let Some(eta) = p.eta_ms {
        extra.push(format!("ETA {}", clock(eta)));
    }
    text.push_str(&paint.dim(&format!(" · {}", extra.join(" · "))));
    Some(Line {
        pos: p.embeddings_computed,
        len: (known > 0).then_some(known),
        text,
    })
}

/// `38`, or `2.5` below ten, where a whole number would read as stalled.
fn rate(per_second: f64) -> String {
    if per_second < 10.0 {
        format!("{per_second:.1}")
    } else {
        format!("{per_second:.0}")
    }
}

/// `0:42`, `12:05`, `1:02:09`.
pub(crate) fn clock(ms: u64) -> String {
    let s = ms.div_ceil(1000);
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `12s`, `4m 10s`: how long ago something started.
pub(crate) fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        _ => format!("{}m {:02}s", s / 60, s % 60),
    }
}

struct Bars {
    multi: MultiProgress,
    status: ProgressBar,
    files: OnceLock<ProgressBar>,
    embed: OnceLock<ProgressBar>,
    paint: Paint,
}

/// The board on stderr. Cloned cheaply; every clone draws the same lines.
#[derive(Clone)]
pub(crate) struct Board(Arc<Bars>);

impl Board {
    /// A board, or nothing when stderr is not a terminal: a pipe or a log
    /// gets the final summary line, not hundreds of carriage returns.
    pub(crate) fn new() -> Option<Self> {
        if !std::io::stderr().is_terminal() {
            return None;
        }
        let multi = MultiProgress::new();
        let status = multi.add(ProgressBar::new_spinner());
        status.set_style(style("{spinner} {msg}"));
        status.enable_steady_tick(Duration::from_millis(120));
        Some(Self(Arc::new(Bars {
            multi,
            status,
            files: OnceLock::new(),
            embed: OnceLock::new(),
            paint: Paint::for_stderr(),
        })))
    }

    /// A line printed above the board, kept once the board is gone.
    pub(crate) fn note(&self, text: &str) {
        // A note that cannot be drawn is not worth failing a sync over.
        let _ = self.0.multi.println(text);
    }

    /// The job waits for a slot behind other syncs.
    pub(crate) fn queued(&self) {
        self.0.status.set_message("queued — waiting for a slot");
    }

    pub(crate) fn scanning(&self, seen: u64) {
        self.0.status.set_message(scanning(seen));
    }

    pub(crate) fn draw(&self, p: &Progress) {
        match view(p, self.0.paint) {
            View::Scanning(text) => self.0.status.set_message(text),
            View::Working { files, embed } => {
                self.0.status.finish_and_clear();
                self.set(&self.0.files, "files", &files);
                if let Some(embed) = embed {
                    self.set(&self.0.embed, "embed", &embed);
                }
            }
        }
    }

    /// Clears every line: the summary printed next replaces them.
    pub(crate) fn clear(&self) {
        self.0.status.finish_and_clear();
        for bar in [self.0.files.get(), self.0.embed.get()]
            .into_iter()
            .flatten()
        {
            bar.finish_and_clear();
        }
    }

    fn set(&self, slot: &OnceLock<ProgressBar>, label: &str, line: &Line) {
        let bar = slot.get_or_init(|| {
            let bar = self.0.multi.add(ProgressBar::new(0));
            bar.set_prefix(self.0.paint.bold(label));
            bar
        });
        match line.len {
            Some(len) => {
                bar.set_style(style("{prefix} {bar:28} {msg}"));
                bar.set_length(len);
            }
            None => bar.set_style(style("{prefix} {msg}")),
        }
        bar.set_position(line.pos);
        bar.set_message(line.text.clone());
    }
}

fn style(template: &str) -> ProgressStyle {
    // The templates are constants: one that does not parse is a bug caught by
    // the tests below, and the default style is still a working bar.
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned() -> Progress {
        Progress {
            phase: ProgressPhase::Indexing,
            files_seen: 7200,
            files_total: 7200,
            files_unchanged: 6888,
            files_pending: 312,
            ..Progress::default()
        }
    }

    fn text(line: &Line) -> &str {
        &line.text
    }

    #[test]
    fn the_walk_shows_a_count_before_any_total_exists() {
        let p = Progress {
            files_seen: 4120,
            ..Progress::default()
        };
        assert_eq!(
            view(&p, Paint::plain()),
            View::Scanning("scanning… 4 120 files".into())
        );
    }

    #[test]
    fn mid_embed_both_lines_carry_their_own_denominator() {
        let mut p = planned();
        p.files_done = 96;
        p.expect_embeddings(2300);
        p.embeddings_cached = 5937;
        p.elapsed_ms = 40_553;
        p.observe(1541);
        let View::Working { files, embed } = view(&p, Paint::plain()) else {
            panic!("planned");
        };
        assert_eq!(text(&files), "96/312 · 6 888 unchanged");
        assert_eq!((files.pos, files.len), (96, Some(312)));
        let embed = embed.expect("an embedding line once there are misses");
        assert_eq!(text(&embed), "1 541/2 300 · 5 937 cached · 38/s · ETA 0:20");
        assert_eq!((embed.pos, embed.len), (1541, Some(2300)));
    }

    #[test]
    fn a_pass_that_embeds_nothing_has_no_embedding_line() {
        let mut p = planned();
        p.files_unchanged = 7200;
        p.files_pending = 0;
        let View::Working { files, embed } = view(&p, Paint::plain()) else {
            panic!("planned");
        };
        assert_eq!(text(&files), "0/0 · 7 200 unchanged");
        assert!(embed.is_none());
    }

    #[test]
    fn a_count_without_a_denominator_draws_no_bar() {
        let mut p = planned();
        p.files_unchanged = 0;
        p.elapsed_ms = 2_000;
        p.observe(64);
        p.embeddings_cached = 10;
        let View::Working { embed, .. } = view(&p, Paint::plain()) else {
            panic!("planned");
        };
        let embed = embed.expect("line");
        assert_eq!(embed.len, None);
        assert_eq!(text(&embed), "64 · 10 cached · 32/s");
    }

    #[test]
    fn pruning_is_named_on_the_files_line() {
        let mut p = planned();
        p.phase = ProgressPhase::Pruning;
        p.files_done = 312;
        let View::Working { files, .. } = view(&p, Paint::plain()) else {
            panic!("planned");
        };
        assert_eq!(text(&files), "312/312 · 6 888 unchanged · pruning");
    }

    #[test]
    fn durations_read_as_a_clock_and_as_an_age() {
        assert_eq!(clock(42_000), "0:42");
        assert_eq!(clock(41_001), "0:42", "an ETA rounds up");
        assert_eq!(clock(3_729_000), "1:02:09");
        assert_eq!(ago(12_400), "12s");
        assert_eq!(ago(250_000), "4m 10s");
        assert_eq!(rate(2.54), "2.5");
    }

    #[test]
    fn every_template_parses() {
        for template in [
            "{spinner} {msg}",
            "{prefix} {bar:28} {msg}",
            "{prefix} {msg}",
        ] {
            assert!(ProgressStyle::with_template(template).is_ok(), "{template}");
        }
    }
}
