//! The closed vocabularies a flag accepts, advertised on the argument surface.
//!
//! INTENT: `--lang`, `--kind` and `--task` take a value from a list
//! `findery-core` owns. The list belongs in `--help` and under TAB, so the
//! surface must know it — but the check stays where it was, in
//! `search::request`, which refuses an unknown value with findery's own error
//! naming the valid ones. [`Vocabulary`] therefore advertises without
//! validating: one check, one message, and the help can never list a value the
//! check would refuse, because both read the same `ALL`.

use std::ffi::OsStr;

use clap::builder::{PossibleValue, StringValueParser, TypedValueParser};
use clap::{Arg, Command};

use findery_core::intent::QueryTask;
use findery_core::model::{ChunkKind, Lang};

/// Every language a chunk can be filtered on.
pub(crate) fn langs() -> Vec<&'static str> {
    Lang::ALL.iter().map(Lang::as_str).collect()
}

/// Every chunk kind a search can be narrowed to.
pub(crate) fn kinds() -> Vec<&'static str> {
    ChunkKind::ALL.iter().map(ChunkKind::as_str).collect()
}

/// Every legacy task name `--task` takes.
pub(crate) fn tasks() -> Vec<&'static str> {
    QueryTask::ALL.iter().map(QueryTask::as_str).collect()
}

/// A value parser that passes text through and lists the vocabulary it names.
#[derive(Clone, Copy)]
pub(crate) struct Vocabulary(fn() -> Vec<&'static str>);

impl Vocabulary {
    pub(crate) fn langs() -> Self {
        Self(langs)
    }

    pub(crate) fn kinds() -> Self {
        Self(kinds)
    }

    pub(crate) fn tasks() -> Self {
        Self(tasks)
    }
}

impl TypedValueParser for Vocabulary {
    type Value = String;

    fn parse_ref(
        &self,
        cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<String, clap::Error> {
        StringValueParser::new().parse_ref(cmd, arg, value)
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new((self.0)().into_iter().map(PossibleValue::new)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advertised(vocabulary: Vocabulary) -> Vec<String> {
        vocabulary
            .possible_values()
            .expect("a vocabulary always advertises")
            .map(|v| v.get_name().to_string())
            .collect()
    }

    #[test]
    fn the_help_lists_exactly_what_core_accepts() {
        assert_eq!(advertised(Vocabulary::langs()), langs());
        assert_eq!(advertised(Vocabulary::kinds()), kinds());
        assert_eq!(advertised(Vocabulary::tasks()), tasks());
        for name in langs() {
            assert!(Lang::parse(name).is_some(), "{name} must parse back");
        }
    }

    /// The domain check, not clap, refuses an unknown value — so the refusal
    /// keeps findery's wording and its list of valid values.
    #[test]
    fn an_unknown_value_still_reaches_the_domain_check() {
        let cmd = Command::new("t");
        let parsed = Vocabulary::langs()
            .parse_ref(&cmd, None, OsStr::new("cobol"))
            .expect("passed through");
        assert_eq!(parsed, "cobol");
    }
}
