//! `findery search`: the arguments, the request they make, and the answer
//! printed.
//!
//! Every value that names something the daemon knows — a task, a language, a
//! kind — is checked against the vocabulary in `findery-core`, and an unknown
//! one is refused with the list of valid ones, never dropped.

use clap::Args;
use clap::builder::PossibleValuesParser;

use findery_core::embedding::prompt::QueryTask;
use findery_core::ids::ProjectId;
use findery_core::model::{ChunkKind, Lang, SearchFilter, SearchMode, SearchRequest};
use findery_core::{Error, Result};

use findery_client::Client;

use crate::args::Scope;
use crate::ui::render;
use crate::workspace::scope::{parse_mode, resolve};

/// `findery search`.
#[derive(Args)]
pub(crate) struct SearchArgs {
    /// What to look for, in words
    #[arg(value_name = "QUERY", required = true)]
    pub(crate) query: Vec<String>,
    #[command(flatten)]
    pub(crate) scope: Scope,
    /// Hits to return [default: `search.default_k`]
    #[arg(short, long, value_name = "N")]
    pub(crate) k: Option<usize>,
    /// Ranking: by meaning, by exact words, or both fused
    #[arg(long, value_name = "MODE", value_parser = modes())]
    pub(crate) mode: Option<String>,
    /// A named search task: `code`, `symbol`, `usage`, `docs`, `test`,
    /// `config`, `error`, or one of their aliases
    #[arg(long, value_name = "TASK")]
    pub(crate) task: Option<String>,
    /// Your own instruction for the model, replacing the task's
    ///
    /// The first half of what the model reads as `Instruct: …` before
    /// `Query: …`; the query is the other half.
    #[arg(long, alias = "instruct", value_name = "TEXT")]
    pub(crate) instruction: Option<String>,
    /// Only chunks whose symbol contains this text
    #[arg(long, value_name = "TEXT")]
    pub(crate) symbol: Option<String>,
    /// A named restriction from `.findery/config.toml`, applied before scoring
    ///
    /// Never a second index, never a cut applied after the ranking.
    #[arg(long = "scope", value_name = "SCOPE")]
    pub(crate) search_scope: Option<String>,
    /// Only paths matching this glob; repeatable
    #[arg(long = "path", value_name = "GLOB")]
    pub(crate) paths: Vec<String>,
    /// Only this language; repeatable
    #[arg(long = "lang", value_name = "LANG")]
    pub(crate) langs: Vec<String>,
    /// Only this kind of chunk; repeatable
    #[arg(long = "kind", value_name = "KIND")]
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

/// Runs a search and prints it: hits on stdout, the warnings on stderr, so a
/// script reading stdout keeps reading hits.
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
    render::hits(&response.hits);
    Ok(())
}

/// The request `args` describe, every name checked first.
fn request(project: ProjectId, args: SearchArgs) -> Result<SearchRequest> {
    let langs: Vec<_> = Lang::ALL.iter().map(Lang::as_str).collect();
    let kinds: Vec<_> = ChunkKind::ALL.iter().map(ChunkKind::as_str).collect();
    let filter = SearchFilter {
        paths: args.paths,
        langs: parse_all(&args.langs, "language", Lang::parse, &langs)?,
        kinds: parse_all(&args.kinds, "kind", ChunkKind::parse, &kinds)?,
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
        instruction: args.instruction,
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
        let names: Vec<_> = QueryTask::ALL.iter().map(QueryTask::as_str).collect();
        Error::Invalid(format!(
            "unknown task `{raw}`; one of {} (or an alias)",
            names.join(", ")
        ))
    })
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
    fn the_filters_reach_the_request() {
        let args = parsed(&[
            "findery", "search", "--symbol", "retry", "--lang", "rust", "--kind", "function",
            "where", "is", "retry",
        ]);
        let req = request(demo(), args).expect("valid");
        assert_eq!(req.filter.symbol_contains.as_deref(), Some("retry"));
        assert_eq!(req.filter.langs, vec![Lang::Rust]);
        assert_eq!(req.filter.kinds, vec![ChunkKind::Function]);
        assert_eq!(req.query, "where is retry");
        assert_eq!(req.k, None, "the daemon's default k applies");
    }

    #[test]
    fn a_task_alias_is_accepted_and_a_query_is_required() {
        let args = parsed(&["findery", "search", "--task", "callers", "q"]);
        assert_eq!(
            request(demo(), args).expect("alias").task,
            Some(QueryTask::Usage)
        );
        assert!(Cli::try_parse_from(["findery", "search"]).is_err());
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
