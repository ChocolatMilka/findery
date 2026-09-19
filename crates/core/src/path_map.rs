//! Host <-> container path translation.
//!
//! INTENT: the daemon walks `/workspaces/my-app/src/lib.rs` while the human and
//! Claude Code only know `C:/Users/me/RustroverProjects/my-app/src/lib.rs`. A
//! hit citing a container path is a hit nobody can open, so translation is part
//! of the contract, not an afterthought in the UI.

use serde::{Deserialize, Serialize};

/// One bind mount declared in the configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    /// Path as the human sees it (`C:/Users/me/RustroverProjects`).
    pub host: String,
    /// Path as the daemon sees it (`/workspaces`).
    pub container: String,
}

/// Bidirectional translator over a set of mounts.
///
/// Mounts are sorted by descending container-path length so that the most
/// specific mount always wins: `/workspaces/app` is matched before
/// `/workspaces`.
#[derive(Clone, Debug, Default)]
pub struct PathMapper {
    mounts: Vec<Mount>,
}

impl PathMapper {
    pub fn new(mut mounts: Vec<Mount>) -> Self {
        for m in &mut mounts {
            m.host = normalize(&m.host);
            m.container = normalize(&m.container);
        }
        mounts.sort_by(|a, b| b.container.len().cmp(&a.container.len()));
        Self { mounts }
    }

    /// Identity mapper: the daemon runs natively, host and container agree.
    pub fn identity() -> Self {
        Self { mounts: Vec::new() }
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    /// Container path -> host path. Returns the input unchanged when no mount
    /// matches, because a wrong translation is worse than none.
    pub fn to_host(&self, container_path: &str) -> String {
        let p = normalize(container_path);
        for m in &self.mounts {
            if let Some(rest) = strip_prefix_path(&p, &m.container, false) {
                return join(&m.host, rest);
            }
        }
        p
    }

    /// Host path -> container path. Host comparison is case-insensitive so that
    /// `C:/Users` and `c:/users` map identically on Windows.
    pub fn to_container(&self, host_path: &str) -> String {
        let p = normalize(host_path);
        let mut best: Option<(usize, String)> = None;
        for m in &self.mounts {
            if let Some(rest) = strip_prefix_path(&p, &m.host, true) {
                let candidate = join(&m.container, rest);
                if best.as_ref().is_none_or(|(len, _)| m.host.len() > *len) {
                    best = Some((m.host.len(), candidate));
                }
            }
        }
        best.map(|(_, v)| v).unwrap_or(p)
    }
}

/// Normalizes separators and trims a trailing slash (but keeps a bare `/`).
pub fn normalize(path: &str) -> String {
    let mut s = path.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// Joins a prefix and a relative remainder without doubling the separator.
fn join(prefix: &str, rest: &str) -> String {
    if rest.is_empty() {
        return prefix.to_string();
    }
    if prefix.ends_with('/') {
        format!("{prefix}{rest}")
    } else {
        format!("{prefix}/{rest}")
    }
}

/// True when `child` is `parent` itself or sits underneath it.
///
/// Segment-aware and case-insensitive, so `C:/Dev` contains `c:/dev/app` but
/// not `C:/Dev-old`. This is how a client decides which registered project the
/// current working directory belongs to.
pub fn is_within(parent: &str, child: &str) -> bool {
    let (parent, child) = (normalize(parent), normalize(child));
    strip_prefix_path(&child, &parent, true).is_some()
}

/// Strips `prefix` from `path` at a path-segment boundary.
///
/// INTENT: a raw `starts_with` would match `/workspaces-old` against
/// `/workspaces`. The boundary check is the whole point of this helper.
fn strip_prefix_path<'a>(path: &'a str, prefix: &str, ignore_case: bool) -> Option<&'a str> {
    if prefix.is_empty() {
        return None;
    }
    let head = path.get(..prefix.len())?;
    let matches = if ignore_case {
        head.eq_ignore_ascii_case(prefix)
    } else {
        head == prefix
    };
    if !matches {
        return None;
    }
    match path.as_bytes().get(prefix.len()) {
        None => Some(""),
        Some(b'/') => path.get(prefix.len() + 1..),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapper() -> PathMapper {
        PathMapper::new(vec![
            Mount {
                host: "C:\\Users\\me\\Projects".into(),
                container: "/workspaces".into(),
            },
            Mount {
                host: "D:\\scratch".into(),
                container: "/workspaces/scratch".into(),
            },
        ])
    }

    #[test]
    fn container_to_host_uses_the_most_specific_mount() {
        let m = mapper();
        assert_eq!(
            m.to_host("/workspaces/app/src/lib.rs"),
            "C:/Users/me/Projects/app/src/lib.rs"
        );
        assert_eq!(m.to_host("/workspaces/scratch/a.rs"), "D:/scratch/a.rs");
    }

    #[test]
    fn host_to_container_is_case_insensitive() {
        let m = mapper();
        assert_eq!(
            m.to_container("c:/users/me/projects/app"),
            "/workspaces/app"
        );
    }

    #[test]
    fn unmatched_paths_pass_through_untouched() {
        let m = mapper();
        assert_eq!(m.to_host("/elsewhere/x.rs"), "/elsewhere/x.rs");
        assert_eq!(
            m.to_container("/workspaces-old/x.rs"),
            "/workspaces-old/x.rs"
        );
    }
}
