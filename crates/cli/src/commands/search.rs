//! `findery search` and `findery intents`: the arguments, the request they
//! make, and the answer printed.
//!
//! Every value that names something the daemon knows — an intent, a task, a
//! language, a kind — is checked against the vocabulary in `findery-core`, and
//! an unknown one is refused with the list of valid ones, never dropped.

use clap::Args;
use clap::builder::PossibleValuesParser;
use clap_complete::ArgValueCandidates;

use findery_core::ids::ProjectId;
use findery_core::intent::QueryTask;
use findery_core::model::{ChunkKind, Lang, SearchFilter, SearchMode, SearchRequest};
use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::Scope;
use crate::args::complete::values;
use crate::args::vocab::{self, Vocabulary};
use crate::ui::render;
use crate::ui::search_view as view;
use crate::ui::style::Paint;
use crate::workspace::scope::{parse_mode, resolve};

/// `findery search`.
//
// Every value that names something the server knows carries its completer or
// its vocabulary, so TAB and `--help` offer what `request` would accept.
#[derive(Args)]
pub(crate) struct SearchArgs {
    /// What to look for, in words; optional with --like
    #[arg(value_name = "QUERY", required_unless_present = "like")]
    pub(crate) query: Vec<String>,
    #[command(flatten)]
    pub(crate) scope: Scope,
    /// Hits per list [default: `search.default_k`, or each leg's own]
    #[arg(short, long, value_name = "N")]
    pub(crate) k: Option<usize>,
    /// Ranking: by meaning, by exact words, or both fused
    #[arg(long, value_name = "MODE", value_parser = modes())]
    pub(crate) mode: Option<String>,
    /// A named search task: `usage`, `review`, `doc-sync`… (`findery intents`)
    #[arg(long, value_name = "INTENT", add = ArgValueCandidates::new(values::intents))]
    pub(crate) intent: Option<String>,
    /// A legacy task name, or one of its aliases; same as --intent
    #[arg(long, value_name = "TASK", value_parser = Vocabulary::tasks())]
    pub(crate) task: Option<String>,
    /// Your own instruction for the model, replacing the intent's
    ///
    /// The first half of what the model reads as `Instruct: …` before
    /// `Query: …`; the query is the other half.
    #[arg(long, alias = "instruct", value_name = "TEXT")]
    pub(crate) instruction: Option<String>,
    /// Send the query to the model verbatim: no instruction, no template
    #[arg(long)]
    pub(crate) raw: bool,
    /// Look-alikes of a chunk, `path:L12-L48` or a chunk id; embeds nothing
    ///
    /// The chunk's stored vector is the query, and the chunk itself is left
    /// out of the answer.
    #[arg(long, value_name = "LOCATOR")]
    pub(crate) like: Option<String>,
    /// Only chunks whose symbol contains this text
    #[arg(long, value_name = "TEXT")]
    pub(crate) symbol: Option<String>,
    /// A named restriction from `.findery/config.toml`, applied before scoring
    ///
    /// Never a second index, never a cut applied after the ranking.
    #[arg(
        long = "scope",
        value_name = "SCOPE",
        add = ArgValueCandidates::new(values::scopes)
    )]
    pub(crate) search_scope: Option<String>,
    /// Only paths matching this glob; repeatable
    #[arg(long = "path", value_name = "GLOB")]
    pub(crate) paths: Vec<String>,
    /// Only this language; repeatable
    #[arg(long = "lang", value_name = "LANG", value_parser = Vocabulary::langs())]
    pub(crate) langs: Vec<String>,
    /// Only this kind of chunk; repeatable
    #[arg(long = "kind", value_name = "KIND", value_parser = Vocabulary::kinds())]
    pub(crate) kinds: Vec<String>,
    /// Characters kept per excerpt
    #[arg(long, value_name = "N")]
    pub(crate) max_chars: Option<usize>,
}

/// The ranking modes `--mode` takes, validated by clap: an unknown mode has no
/// domain check behind it to fall back on.
fn modes() -> PossibleValuesParser {
    PossibleValuesParser::new(SearchMode::ALL.iter().map(SearchMode::as_str))
}

/// Runs a search and prints it: hits on stdout, the resolution and the
/// warnings on stderr, so a script reading stdout keeps reading hits.
pub(crate) async fn run(client: &Client, args: SearchArgs, json: bool) -> Result<()> {
    let project = resolve(client, &args.scope).await?;
    let request = request(project, args)?;
    let response = client.search(&request).await?;
    for warning in &response.warnings {
        eprintln!("findery: {warning}");
    }
    if json {
        return render::json(&response);
    }
    if let Some(echo) = &response.resolved {
        eprintln!("{}", view::resolved_line(echo, Paint::for_stderr()));
    }
    if response.legs.is_empty() {
        render::hits(&response.hits);
    } else {
        print!("{}", view::grouped(&response, Paint::for_stdout()));
    }
    Ok(())
}

/// The request `args` describe, every name checked first.
fn request(project: ProjectId, args: SearchArgs) -> Result<SearchRequest> {
    let filter = SearchFilter {
        paths: args.paths,
        langs: parse_all(&args.langs, "language", Lang::parse, &vocab::langs())?,
        kinds: parse_all(&args.kinds, "kind", ChunkKind::parse, &vocab::kinds())?,
        symbol_contains: args.symbol,
    };
    let task = match args.task.as_deref() {
        Some(raw) => Some(parse_task(raw)?),
        None => None,
    };
    Ok(SearchRequest {
        project,
        query: args.query.join(" "),
        k: args.k,
        mode: args.mode.as_deref().and_then(parse_mode),
        filter,
        max_snippet_chars: args.max_chars,
        task,
        intent: args.intent,
        instruction: args.instruction,
        raw_query: args.raw,
        like: args.like,
        scope: args.search_scope,
    })
}

/// Every value parsed, or the first unknown one refused with the valid ones.
fn parse_all<T>(
    raw: &[String],
    what: &str,
    parse: impl Fn(&str) -> Option<T>,
    valid: &[&str],
) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(raw.len());
    for value in raw {
        let Some(parsed) = parse(value.trim()) else {
            return Err(Error::Invalid(format!(
                "unknown {what} `{value}`; one of {}",
                valid.join(", ")
            )));
        };
        out.push(parsed);
    }
    Ok(out)
}

fn parse_task(raw: &str) -> Result<QueryTask> {
    QueryTask::parse(raw).ok_or_else(|| {
        Error::Invalid(format!(
            "unknown task `{raw}`; one of {} (or an alias), or any intent with --intent",
            vocab::tasks().join(", ")
        ))
    })
}

/// `findery intents`: the library the current project's searches see — the
/// daemon's alone outside a project.
pub(crate) async fn intents(client: &Client, scope: &Scope, json: bool) -> Result<()> {
    let project = match &scope.project {
        Some(explicit) => Some(ProjectId::new(explicit.clone())?),
        // Outside any project the daemon's library is the answer, not an error.
        None => resolve(client, scope).await.ok(),
    };
    let list = client.intents(project.as_ref()).await?;
    if json {
        return render::json(&list);
    }
    print!("{}", view::intents(&list, Paint::for_stdout()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::args::{Cli, Command};

    fn parsed(argv: &[&str]) -> SearchArgs {
        let cli = Cli::try_parse_from(argv).expect("parse");
        match cli.command {
            Command::Search(args) => *args,
            _ => panic!("expected a search"),
        }
    }

    fn demo() -> ProjectId {
        ProjectId::new("demo").expect("id")
    }

    #[test]
    fn the_new_flags_reach_the_request() {
        let args = parsed(&[
            "findery", "search", "--intent", "review", "--raw", "--symbol", "retry", "--lang",
            "rust", "--kind", "function", "where", "is", "retry",
        ]);
        let req = request(demo(), args).expect("valid");
        assert_eq!(req.intent.as_deref(), Some("review"));
        assert!(req.raw_query);
        assert_eq!(req.filter.symbol_contains.as_deref(), Some("retry"));
        assert_eq!(req.filter.langs, vec![Lang::Rust]);
        assert_eq!(req.filter.kinds, vec![ChunkKind::Function]);
        assert_eq!(req.query, "where is retry");
        assert_eq!(req.k, None, "each leg keeps its own k");
    }

    #[test]
    fn like_needs_no_query_and_a_task_alias_is_accepted() {
        let args = parsed(&["findery", "search", "--like", "src/net.rs:L1-L4"]);
        let req = request(demo(), args).expect("valid");
        assert_eq!(req.like.as_deref(), Some("src/net.rs:L1-L4"));
        assert!(req.query.is_empty());

        let args = parsed(&["findery", "search", "--task", "callers", "q"]);
        assert_eq!(
            request(demo(), args).expect("alias").task,
            Some(QueryTask::Usage)
        );
        assert!(
            Cli::try_parse_from(["findery", "search"]).is_err(),
            "a query or --like"
        );
    }

    #[test]
    fn an_unknown_language_kind_or_task_is_refused_with_the_valid_ones() {
        for (flag, value, needle) in [
            ("--lang", "cobol", "markdown"),
            ("--kind", "class", "doc_section"),
            ("--task", "telepathy", "symbol"),
        ] {
            let args = parsed(&["findery", "search", flag, value, "q"]);
            let err = request(demo(), args).expect_err(value);
            assert_eq!(err.code(), "invalid");
            let text = err.to_string();
            assert!(text.contains(value) && text.contains(needle), "{text}");
        }
    }
}
