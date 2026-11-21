//! The names a TAB press offers: what this machine knows offline, then what a
//! server that is already running adds.
//!
//! INTENT: completion runs on every keystroke that asks for it, so it must be
//! fast and it must have no side effect. Two rules follow:
//!
//! * **Never start anything.** A server is asked only if it already answers,
//!   once, within [`ASK_TIMEOUT`]; there is no path from here to the code that
//!   spawns one, whatever `FINDERY_NO_SPAWN` says. A TAB that boots a process
//!   would make the shell hang for seconds on a word.
//! * **Offline first.** The manifest above the current directory, the settings
//!   file and the built-in intent library answer most questions with no server
//!   at all, so a cold machine still completes `--intent` and `--scope`.
//!
//! Every failure here is silent on purpose: a completer has no channel back to
//! the person but the candidates, and a warning printed into a shell's
//! completion buffer would be typed as if it were a value.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use clap_complete::CompletionCandidate;

use findery_core::Result;
use findery_core::config::{Config, user_config_path};

use findery_client::Client;
use findery_client::resolve::{FoundManifest, discover_manifest};

use crate::settings::endpoint::Endpoint;

/// How long one TAB press waits on a running server before answering without it.
const ASK_TIMEOUT: Duration = Duration::from_millis(300);

/// Project ids: the manifest's, then the server's.
pub(crate) fn projects() -> Vec<CompletionCandidate> {
    Sources::from_env().projects()
}

/// Bucket names: the manifest's, then the server's.
pub(crate) fn buckets() -> Vec<CompletionCandidate> {
    Sources::from_env().buckets()
}

/// Model aliases: the settings file's profiles, then the server's.
pub(crate) fn models() -> Vec<CompletionCandidate> {
    Sources::from_env().models()
}

/// Intent names: the library this project's searches see, offline.
pub(crate) fn intents() -> Vec<CompletionCandidate> {
    Sources::from_env().intents()
}

/// Scope names the manifest declares.
pub(crate) fn scopes() -> Vec<CompletionCandidate> {
    Sources::from_env().scopes()
}

/// The short setting names `config set` and `config unset` take.
pub(crate) fn settings() -> Vec<CompletionCandidate> {
    let mut out = Candidates::default();
    for (short, full) in crate::settings::ALIASES {
        out.push(short, Some(full));
    }
    out.0
}

/// Where the names come from, read once per TAB press.
struct Sources {
    manifest: Option<FoundManifest>,
    settings: Config,
    server: Option<Endpoint>,
}

impl Sources {
    fn from_env() -> Self {
        let settings = std::fs::read_to_string(user_config_path())
            .ok()
            .and_then(|src| Config::from_toml_str(&src).ok())
            .unwrap_or_default();
        let cwd = std::env::current_dir().unwrap_or_default();
        Self::new(cwd, settings, Endpoint::from_env().ok())
    }

    /// Sources for this directory, these settings and this server, if any.
    fn new(cwd: PathBuf, settings: Config, server: Option<Endpoint>) -> Self {
        Self {
            manifest: discover_manifest(&cwd).ok().flatten(),
            settings,
            server,
        }
    }

    fn projects(&self) -> Vec<CompletionCandidate> {
        let mut out = Candidates::default();
        if let Some(found) = &self.manifest {
            out.push(&found.config.project, Some("this directory"));
        }
        for project in self.ask(|c| async move { c.list_projects().await }) {
            out.push(project.id.as_str(), Some(&project.host_root));
        }
        out.0
    }

    fn buckets(&self) -> Vec<CompletionCandidate> {
        let mut out = Candidates::default();
        if let Some(name) = self
            .manifest
            .as_ref()
            .and_then(|m| m.config.bucket.name.as_ref())
        {
            out.push(name, Some("this project's bucket"));
        }
        for bucket in self.ask(|c| async move { c.list_buckets().await }) {
            out.push(&bucket.name, Some(&bucket.path));
        }
        out.0
    }

    fn models(&self) -> Vec<CompletionCandidate> {
        let mut out = Candidates::default();
        for (alias, profile) in self.settings.profiles().unwrap_or_default() {
            out.push(alias.as_str(), Some(&profile.model));
        }
        for model in self.ask(|c| async move { c.models().await }) {
            out.push(model.alias.as_str(), Some(&model.model));
        }
        out.0
    }

    fn intents(&self) -> Vec<CompletionCandidate> {
        let overrides = self.manifest.as_ref().map(|m| &m.config.intents);
        let library = self
            .settings
            .intent_library()
            .and_then(|lib| match overrides {
                Some(layer) => lib.merged(layer),
                None => Ok(lib),
            });
        let mut out = Candidates::default();
        for intent in library.iter().flat_map(|lib| lib.iter()) {
            out.push(&intent.name, Some(&intent.summary));
        }
        out.0
    }

    fn scopes(&self) -> Vec<CompletionCandidate> {
        let mut out = Candidates::default();
        for (name, filter) in self.manifest.iter().flat_map(|m| &m.config.scopes) {
            out.push(name, Some(&filter.paths.join(", ")));
        }
        out.0
    }

    /// One call to the server, when one is configured and answers in time;
    /// nothing otherwise.
    fn ask<T, Fut>(&self, call: impl FnOnce(Client) -> Fut) -> Vec<T>
    where
        Fut: Future<Output = Result<Vec<T>>>,
    {
        let Some(server) = &self.server else {
            return Vec::new();
        };
        let Some(client) = client(server) else {
            return Vec::new();
        };
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return Vec::new();
        };
        // The timer is built inside the runtime: a `timeout` created before
        // `block_on` has no clock to register with, and panics.
        let answer =
            runtime.block_on(async { tokio::time::timeout(ASK_TIMEOUT, call(client)).await });
        match answer {
            Ok(Ok(list)) => list,
            _ => Vec::new(),
        }
    }
}

/// A client that gives up after [`ASK_TIMEOUT`].
fn client(server: &Endpoint) -> Option<Client> {
    let client = Client::new(&server.base_url)
        .ok()?
        .with_timeout(ASK_TIMEOUT)
        .ok()?;
    Some(match &server.token {
        Some(token) => client.with_token(token.as_str()),
        None => client,
    })
}

/// Candidates in first-seen order, each value once: the local answer comes
/// first and keeps its description when the server names the same thing.
#[derive(Default)]
struct Candidates(Vec<CompletionCandidate>);

impl Candidates {
    fn push(&mut self, value: &str, help: Option<&str>) {
        if value.is_empty() || self.0.iter().any(|c| c.get_value() == value) {
            return;
        }
        let help = help.filter(|h| !h.is_empty()).map(|h| h.to_string().into());
        self.0.push(CompletionCandidate::new(value).help(help));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use findery_core::manifest::{BucketRef, ProjectConfig};
    use findery_core::model::SearchFilter;

    use super::*;

    /// A loopback address nothing listens on: bound, read, released.
    fn dead_server() -> Endpoint {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("its address").port();
        drop(listener);
        Endpoint {
            base_url: format!("http://127.0.0.1:{port}"),
            token: None,
        }
    }

    fn names(list: Vec<CompletionCandidate>) -> Vec<String> {
        list.iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect()
    }

    /// A project `axle` in bucket `work`, with a scope `api`.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = ProjectConfig {
            project: "axle".into(),
            bucket: BucketRef {
                name: Some("work".into()),
                path: None,
            },
            ..ProjectConfig::default()
        };
        config.scopes.insert(
            "api".into(),
            SearchFilter {
                paths: vec!["crates/server/**".into()],
                ..SearchFilter::default()
            },
        );
        crate::workspace::manifest::write(dir.path(), &config).expect("manifest");
        dir
    }

    #[test]
    fn intents_are_offered_offline_and_no_server_is_started() {
        let dir = project();
        let server = dead_server();
        let url = server.base_url.clone();
        let started = Instant::now();
        let sources = Sources::new(dir.path().to_path_buf(), Config::default(), Some(server));

        let intents = names(sources.intents());
        assert!(intents.contains(&"usage".into()), "{intents:?}");
        assert!(intents.contains(&"doc-sync".into()), "{intents:?}");
        assert_eq!(
            names(sources.projects()),
            ["axle"],
            "the manifest still answers"
        );
        assert!(names(sources.buckets()).contains(&"work".into()));

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a dead server costs at most a timeout, never a boot"
        );
        let probe = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(findery_client::daemon::probe(&url));
        assert!(!probe, "a TAB press must never start a server");
    }

    #[test]
    fn the_manifest_above_the_directory_names_the_project_bucket_and_scopes() {
        let dir = project();
        let deep = dir.path().join("crates").join("core");
        std::fs::create_dir_all(&deep).expect("mkdir");
        let sources = Sources::new(deep, Config::default(), None);
        assert_eq!(names(sources.projects()), ["axle"]);
        assert_eq!(names(sources.buckets()), ["work"]);
        assert_eq!(names(sources.scopes()), ["api"]);
    }

    #[test]
    fn model_aliases_come_from_the_settings_profiles() {
        let settings = Config::from_toml_str("[models.harrier]\nmodel = \"harrier-oss-v1-0.6b\"\n")
            .expect("settings");
        let sources = Sources::new(PathBuf::new(), settings, None);
        assert_eq!(names(sources.models()), ["embedder", "harrier"]);
    }

    #[test]
    fn outside_any_project_only_the_offline_vocabularies_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sources = Sources::new(dir.path().to_path_buf(), Config::default(), None);
        assert!(sources.projects().is_empty());
        assert!(sources.scopes().is_empty());
        assert!(!sources.intents().is_empty());
        assert!(names(settings()).contains(&"model".into()));
    }

    #[test]
    fn a_name_is_offered_once_with_its_first_description() {
        let mut out = Candidates::default();
        out.push("axle", Some("this directory"));
        out.push("axle", Some("D:/work/axle"));
        out.push("", Some("nothing"));
        assert_eq!(out.0.len(), 1);
        let help = out.0[0].get_help().map(ToString::to_string);
        assert_eq!(help.as_deref(), Some("this directory"));
    }

    /// The rule above, made mechanical: the code that starts a server is not
    /// reachable from completion, so no later edit can make TAB boot one.
    #[test]
    fn completion_has_no_path_to_the_code_that_starts_a_server() {
        let spawn = concat!("ensure", "_running");
        for (file, source) in [
            ("values.rs", include_str!("values.rs")),
            ("mod.rs", include_str!("mod.rs")),
        ] {
            assert!(!source.contains(spawn), "{file} reaches for the spawn path");
        }
    }
}
