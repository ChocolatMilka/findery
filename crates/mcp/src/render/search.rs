//! A search answer, as the model reads it.
//!
//! ````text
//! note: keyword results only: …                    ← warnings first
//! 8 hits · axle · intent usage · "Given an API…" · model harrier
//! root C:/work/axle                                ← once, not per hit
//!
//! [1] src/net.rs:L12-L48 · method Client > retry
//! ```rust
//! …
//! ```
//! ````
//!
//! A workflow answers one `## <leg>` section per search it ran, then its pairs
//! (`possibly drifted`) and its near-duplicates, each as locators. The fused
//! score is never printed: an RRF value means nothing on its own, the rank is
//! what it decides.

use findery_core::api::{Duplicate, EchoLeg, Pairing, ResolvedEcho, SearchResponse};
use findery_core::ids::ProjectId;
use findery_core::model::{Chunk, SearchHit};

use super::{clip, fenced, notes};

/// How much of each hit an answer prints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Detail {
    /// A workflow shows its top hits per leg in full, the rest as one line.
    #[default]
    Compact,
    /// Every excerpt.
    Full,
}

impl Detail {
    /// The name `detail` takes on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }

    /// Reads a name [`Detail::as_str`] gives.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "compact" => Some(Self::Compact),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// Excerpts a compact workflow answer shows in full, per leg.
const EXCERPTS_PER_LEG: usize = 3;
/// Longest instruction the header quotes.
const HEADER_INSTRUCTION_CHARS: usize = 90;
/// Longest first line a one-line hit quotes.
const ONE_LINE_CHARS: usize = 100;

/// The whole answer.
pub fn render(
    project: &ProjectId,
    query: &str,
    response: &SearchResponse,
    detail: Detail,
) -> String {
    let all: Vec<&SearchHit> = every_hit(response).collect();
    let root = common_root(&all);
    let mut out = notes(&response.warnings);
    out.push_str(&header(project, all.len(), response.resolved.as_ref()));
    if let Some(root) = &root {
        out.push_str(&format!("root {root}\n"));
    }
    if all.is_empty() {
        out.push_str(&nothing_found(project, query));
        return out;
    }
    let shown = Shown {
        root: root.as_deref(),
    };
    if response.legs.is_empty() {
        out.push_str(&shown.hits(&response.hits, usize::MAX));
        return out;
    }
    let in_full = match detail {
        Detail::Compact => EXCERPTS_PER_LEG,
        Detail::Full => usize::MAX,
    };
    for leg in &response.legs {
        out.push_str(&format!("\n## {} — {}\n", leg.label, count(leg.hits.len())));
        out.push_str(&shown.hits(&leg.hits, in_full));
    }
    out.push_str(&pairs(response));
    out.push_str(&duplicates(response));
    out
}

fn every_hit(response: &SearchResponse) -> impl Iterator<Item = &SearchHit> {
    let legs = response.legs.iter().flat_map(|leg| leg.hits.iter());
    let single = response.legs.is_empty().then_some(response.hits.iter());
    legs.chain(single.into_iter().flatten())
}

/// `8 hits · axle · intent usage · "Given …" · model harrier`.
fn header(project: &ProjectId, hits: usize, echo: Option<&ResolvedEcho>) -> String {
    let mut parts = vec![count(hits), format!("`{project}`")];
    if let Some(echo) = echo {
        parts.extend(
            echo.intent
                .as_ref()
                .map(|intent| format!("intent {intent}")),
        );
        parts.push(prompted(echo));
        parts.extend(echo.model.as_ref().map(|model| format!("model {model}")));
    }
    format!("{}\n", parts.join(" · "))
}

/// How the model was asked: the instruction, or the legs of a workflow.
fn prompted(echo: &ResolvedEcho) -> String {
    match echo.legs.as_slice() {
        [one] => how(one),
        legs => {
            let labels: Vec<&str> = legs.iter().map(|leg| leg.label.as_str()).collect();
            format!("{} legs: {}", legs.len(), labels.join(", "))
        }
    }
}

fn how(leg: &EchoLeg) -> String {
    if leg.by_example {
        return "by example (stored vector)".to_string();
    }
    if leg.raw {
        return "raw query".to_string();
    }
    match &leg.instruction {
        Some(text) => format!("\"{}\"", clip(text, HEADER_INSTRUCTION_CHARS)),
        None => "the model's default instruction".to_string(),
    }
}

fn count(n: usize) -> String {
    match n {
        0 => "no match".to_string(),
        1 => "1 hit".to_string(),
        n => format!("{n} hits"),
    }
}

/// The root every hit's host path shares, when they all share one.
fn common_root(hits: &[&SearchHit]) -> Option<String> {
    let mut roots = hits.iter().map(|hit| root_of(hit));
    let first = roots.next()??;
    roots.all(|r| r.as_deref() == Some(&first)).then_some(first)
}

/// The host path minus the relative one: where the project sits on the host.
fn root_of(hit: &SearchHit) -> Option<String> {
    let root = hit.host_path.strip_suffix(&hit.chunk.path)?;
    Some(root.trim_end_matches('/').to_string())
}

/// How hits are laid out for one answer.
struct Shown<'a> {
    /// The root every hit shares, printed once in the header.
    root: Option<&'a str>,
}

impl Shown<'_> {
    /// The first `in_full` hits with their excerpt, the rest on one line.
    fn hits(&self, hits: &[SearchHit], in_full: usize) -> String {
        hits.iter()
            .enumerate()
            .map(|(i, hit)| match i < in_full {
                true => self.full(i + 1, hit),
                false => self.one_line(i + 1, hit),
            })
            .collect()
    }

    fn full(&self, rank: usize, hit: &SearchHit) -> String {
        let chunk = &hit.chunk;
        let at = locator(chunk);
        let mut out = format!("\n[{rank}] {}\n", self.title(hit));
        out.push_str(&fenced(&chunk.path, chunk.lang, &chunk.text, &at));
        out
    }

    fn one_line(&self, rank: usize, hit: &SearchHit) -> String {
        let first = hit
            .chunk
            .text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty());
        match first {
            Some(line) => format!(
                "[{rank}] {} — {}\n",
                self.title(hit),
                clip(line, ONE_LINE_CHARS)
            ),
            None => format!("[{rank}] {}\n", self.title(hit)),
        }
    }

    /// `src/net.rs:L12-L48 · method Client > retry`, and the host path only
    /// when it is not the shared root plus the relative path.
    fn title(&self, hit: &SearchHit) -> String {
        let chunk = &hit.chunk;
        let mut title = format!("{} · {}", locator(chunk), what(chunk));
        if self.root.is_none() || root_of(hit).as_deref() != self.root {
            title.push_str(&format!(" · {}", hit.host_path));
        }
        title
    }
}

/// `path:L12-L48`, the form `read` and `like` take back.
pub fn locator(chunk: &Chunk) -> String {
    format!("{}:L{}-L{}", chunk.path, chunk.start_line, chunk.end_line)
}

/// `method Client > retry`, or the kind alone.
fn what(chunk: &Chunk) -> String {
    match chunk.breadcrumb.as_deref().or(chunk.symbol.as_deref()) {
        Some(symbol) => format!("{} {symbol}", chunk.kind.as_str()),
        None => chunk.kind.as_str().to_string(),
    }
}

fn nothing_found(project: &ProjectId, query: &str) -> String {
    format!(
        "\nNo match in `{project}` for {query:?}.\nTry: broader words; mode: \"keyword\" for an \
         exact identifier; another intent; `sync` if the code is newer than the index; \
         `read {{glob: \"…\"}}` to check the file is indexed at all.\n"
    )
}

fn pairs(response: &SearchResponse) -> String {
    if response.pairs.is_empty() {
        return String::new();
    }
    let side = |at: usize| response.legs.get(at).map_or("?", |leg| leg.label.as_str());
    let mut out = format!("\n## pairs — {} → {}\n", side(0), side(1));
    for pairing in &response.pairs {
        out.push_str(&pair_line(response, pairing));
    }
    out
}

/// `- README.md:L3-L9 possibly drifted → src/net.rs:L12-L48 (0.41)`.
fn pair_line(response: &SearchResponse, pairing: &Pairing) -> String {
    let Some(left) = response.hit(pairing.left) else {
        return String::new();
    };
    let flag = if pairing.drifted {
        " possibly drifted"
    } else {
        ""
    };
    let matches: Vec<String> = pairing
        .matches
        .iter()
        .filter_map(|m| {
            let right = response.hit(m.right)?;
            Some(format!("{} ({:.2})", locator(&right.chunk), m.similarity))
        })
        .collect();
    let right = match matches.is_empty() {
        true => "no counterpart found".to_string(),
        false => matches.join(", "),
    };
    format!("- {}{flag} → {right}\n", locator(&left.chunk))
}

fn duplicates(response: &SearchResponse) -> String {
    if response.duplicates.is_empty() {
        return String::new();
    }
    let mut out = "\n## near-duplicates — copies that may have diverged\n".to_string();
    for duplicate in &response.duplicates {
        out.push_str(&duplicate_line(response, duplicate));
    }
    out
}

fn duplicate_line(response: &SearchResponse, duplicate: &Duplicate) -> String {
    let (Some(a), Some(b)) = (response.hit(duplicate.a), response.hit(duplicate.b)) else {
        return String::new();
    };
    format!(
        "- {} ≈ {} ({:.2})\n",
        locator(&a.chunk),
        locator(&b.chunk),
        duplicate.similarity
    )
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
