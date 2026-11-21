//! How one `FINDERY_*` variable's raw text is read: numbers, booleans,
//! secrets, templates, lists, headers, mounts.
//!
//! INTENT: split out of [`super::env`], whose table says WHICH variable
//! overrides WHICH key (CONV-029). Reading a value is a separate concern, and
//! every reader carries the variable's name so a parse failure names the thing
//! the user typed.

use std::collections::BTreeMap;

use crate::support::path_map::Mount;
use crate::{Error, Result};

/// A variable's raw text, carrying the variable it came from so that a parse
/// failure can name the thing the user typed.
pub struct EnvValue<'a> {
    pub var: &'a str,
    pub raw: &'a str,
}

impl EnvValue<'_> {
    pub(super) fn num<T: std::str::FromStr>(&self) -> Result<T> {
        self.raw
            .trim()
            .parse::<T>()
            .map_err(|_| Error::Config(format!("{}: `{}` is not a number", self.var, self.raw)))
    }

    pub(super) fn flag(&self) -> Result<bool> {
        match self.raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(Error::Config(format!(
                "{}: `{}` is not a boolean",
                self.var, self.raw
            ))),
        }
    }

    /// An absent secret is an empty string; the overlay clears the field.
    pub(super) fn optional(&self) -> Option<String> {
        (!self.raw.is_empty()).then(|| self.raw.to_string())
    }

    /// A prompt template. A shell or a `.env` file cannot easily carry a
    /// newline, and the default query template has one, so a literal `\n`
    /// stands for it. Empty clears the field back to the default.
    pub(super) fn template(&self) -> Option<String> {
        self.optional().map(|t| t.replace("\\n", "\n"))
    }

    /// `rust,md,ts` — dots and case are tolerated, empties dropped.
    pub(super) fn extensions(&self) -> Vec<String> {
        self.raw
            .split(',')
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    }

    /// `Name: value[;Name: value]*`.
    pub(super) fn headers(&self) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for entry in self.raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (name, value) = entry.split_once(':').ok_or_else(|| {
                Error::Config(format!("{}: `{entry}` is not `Name: value`", self.var))
            })?;
            if name.trim().is_empty() {
                return Err(Error::Config(format!(
                    "{}: `{entry}` has an empty name",
                    self.var
                )));
            }
            out.insert(name.trim().to_string(), value.trim().to_string());
        }
        Ok(out)
    }

    /// `host=container[;host=container]*`.
    pub(super) fn mounts(&self) -> Result<Vec<Mount>> {
        let mut out = Vec::new();
        for entry in self.raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (host, container) = entry.split_once('=').ok_or_else(|| {
                Error::Config(format!("{}: `{entry}` is not `host=container`", self.var))
            })?;
            if host.trim().is_empty() || container.trim().is_empty() {
                return Err(Error::Config(format!(
                    "{}: `{entry}` has an empty side",
                    self.var
                )));
            }
            out.push(Mount {
                host: host.trim().into(),
                container: container.trim().into(),
            });
        }
        Ok(out)
    }
}
