//! Making sure the daemon is up, without the user ever thinking about it.
//!
//! INTENT: a product where you must keep a terminal open is not a product. The
//! CLI and the MCP bridge both call [`ensure_running`] before their first
//! request; if nothing answers on a loopback address, the daemon is started in
//! the background and the command carries on. This is what `rust-analyzer`,
//! `gpg-agent` and `docker` all do, and for the same reason.
//!
//! Two rules keep it honest:
//!
//! * **Only loopback.** A remote URL is someone else's server. Failing to reach
//!   it is an error to report, never a reason to start a local process that
//!   would silently answer from the wrong index.
//! * **Never silent.** The daemon's output goes to a log file next to the
//!   catalog, and a start that does not come up within the deadline fails with
//!   that path in the message.

use crate::Client;
use findery_core::{Error, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long to wait for a freshly started daemon to answer.
///
/// It has to open the catalog, probe the embedding runtime and warm nothing —
/// a couple of seconds on a cold disk, more when the model is still loading.
const STARTUP_DEADLINE: Duration = Duration::from_secs(30);
/// How long to wait on the liveness probe itself.
const PROBE_TIMEOUT: Duration = Duration::from_millis(900);

/// Whether the daemon answers, and starts it if it should.
///
/// Returns `Ok(false)` when it was already up, `Ok(true)` when this call started
/// it. Set `FINDERY_NO_SPAWN=1` to forbid starting one.
pub async fn ensure_running(client: &Client) -> Result<bool> {
    if probe(client.base_url()).await {
        return Ok(false);
    }
    if !is_loopback(client.base_url()) {
        return Err(Error::upstream(
            "findery",
            format!(
                "no daemon answering at {}. It is not a local address, so nothing \
                 was started: check the server, or FINDERY_SERVER_URL",
                client.base_url()
            ),
        ));
    }
    if std::env::var("FINDERY_NO_SPAWN").is_ok_and(|v| v != "0" && !v.is_empty()) {
        return Err(Error::upstream(
            "findery",
            format!(
                "no daemon answering at {} and FINDERY_NO_SPAWN is set; start \
                 `finderyd` yourself",
                client.base_url()
            ),
        ));
    }

    let mut child = spawn()?;
    let started = Instant::now();
    while started.elapsed() < STARTUP_DEADLINE {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if probe(client.base_url()).await {
            return Ok(true);
        }
        // A daemon that refused its configuration is dead within a second.
        // Waiting out the deadline before saying so would turn a one-line fix —
        // a bad model name, a port already taken — into half a minute of
        // nothing, and then a message that does not name the cause.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::upstream(
                "findery",
                format!(
                    "the daemon exited immediately ({status}):\n  {}\n  full log: {}",
                    last_complaint().unwrap_or_else(|| "nothing in the log".into()),
                    log_path()
                ),
            ));
        }
    }
    Err(Error::upstream(
        "findery",
        format!(
            "started the daemon but it did not answer within {}s — see {}",
            STARTUP_DEADLINE.as_secs(),
            log_path()
        ),
    ))
}

/// The last line the daemon wrote that looks like an explanation.
///
/// The daemon prints its fatal error bare on stderr before tracing formats it,
/// so the `findery: ` line is the one written for a human. Falling back to the
/// final line keeps this useful if that ever changes.
fn last_complaint() -> Option<String> {
    let log = std::fs::read_to_string(log_path()).ok()?;
    let tail: Vec<&str> = log.lines().rev().take(40).collect();
    tail.iter()
        .find(|l| l.starts_with("findery: "))
        .or_else(|| tail.iter().find(|l| !l.trim().is_empty()))
        .map(|l| l.trim().to_string())
}

/// Liveness only: `/healthz` needs no token and does not probe the model, so a
/// daemon whose embedding runtime is still warming up still counts as up.
pub async fn probe(base_url: &str) -> bool {
    let Ok(http) = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() else {
        return false;
    };
    http.get(format!("{}/healthz", base_url.trim_end_matches('/')))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// True when the URL points at this machine.
pub fn is_loopback(base_url: &str) -> bool {
    let host = base_url
        .split("//")
        .nth(1)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(
        host,
        "127.0.0.1" | "localhost" | "0.0.0.0" | "[::1]" | "::1"
    ) || host.starts_with("127.")
}

/// Starts the daemon detached, with its output captured to a log file.
///
/// The handle comes back so the caller can notice a daemon that died on its
/// configuration rather than waiting out the startup deadline.
fn spawn() -> Result<std::process::Child> {
    let binary = locate()?;
    let log = PathBuf::from(log_path());
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sink = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| Error::Internal(format!("cannot open {}: {e}", log.display())))?;
    let errors = sink
        .try_clone()
        .map_err(|e| Error::Internal(format!("cannot duplicate log handle: {e}")))?;

    let attempt = |extra: u32| -> std::io::Result<std::process::Child> {
        let _ = extra;
        let mut command = std::process::Command::new(&binary);
        command
            .stdin(std::process::Stdio::null())
            .stdout(sink.try_clone()?)
            .stderr(errors.try_clone()?);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Detached and windowless, or the daemon flashes a console on every
            // single command the user types.
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | extra);
        }
        command.spawn()
    };

    // Terminals that run their commands inside a job object — IDE consoles, CI
    // runners, some task wrappers — kill every process in the job when the
    // command returns, daemon included. Breaking away is the only way out, and a
    // job that forbids it simply refuses the flag, so the plain attempt stays as
    // the fallback rather than replacing it.
    #[cfg(windows)]
    const BREAKAWAY: u32 = 0x0100_0000;

    #[cfg(windows)]
    if let Ok(child) = attempt(BREAKAWAY) {
        return Ok(child);
    }

    attempt(0).map_err(|e| Error::Internal(format!("cannot start {}: {e}", binary.display())))
}

/// Finds `finderyd`, next to this executable first.
///
/// INTENT: a zip or a build directory puts the two binaries side by side, and
/// that pairing is what the user expects — picking a different `finderyd` off
/// the PATH would silently talk to another build.
fn locate() -> Result<PathBuf> {
    let name = if cfg!(windows) {
        "finderyd.exe"
    } else {
        "finderyd"
    };
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    which(name).ok_or_else(|| {
        Error::Internal(format!(
            "`{name}` not found next to this binary nor on PATH; install it or \
             start the daemon yourself"
        ))
    })
}

/// Minimal PATH lookup. A crate for this would be a dependency for ten lines.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Where a self-started daemon writes its output.
///
/// Returned as a string rather than a `PathBuf` because it is printed far more
/// often than it is opened, and `join` on Windows would mix separators into a
/// path the user is meant to paste back into a shell.
pub fn log_path() -> String {
    format!("{}/finderyd.log", findery_core::config::data_dir_from_env())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_addresses_may_be_started() {
        for url in [
            "http://127.0.0.1:8077",
            "http://localhost:8077",
            "http://127.0.0.1",
            "http://[::1]:8077",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in [
            "https://findery.internal:8077",
            "http://10.0.0.5:8077",
            "http://index.example.com",
        ] {
            assert!(!is_loopback(url), "{url} is someone else's server");
        }
    }

    #[tokio::test]
    async fn a_remote_daemon_that_is_down_is_reported_not_replaced() {
        // Starting a local daemon here would answer from an empty index and look
        // like the remote one had lost everything.
        let client = Client::new("http://index.example.com:8077").expect("client");
        let err = ensure_running(&client).await.expect_err("must not spawn");
        assert_eq!(err.code(), "upstream");
        assert!(err.to_string().contains("not a local address"), "{err}");
    }

    #[tokio::test]
    async fn spawning_can_be_forbidden() {
        // SAFETY: single-threaded test, and the variable is removed right after.
        unsafe { std::env::set_var("FINDERY_NO_SPAWN", "1") };
        let client = Client::new("http://127.0.0.1:1").expect("client");
        let err = ensure_running(&client).await.expect_err("must not spawn");
        unsafe { std::env::remove_var("FINDERY_NO_SPAWN") };
        assert!(err.to_string().contains("FINDERY_NO_SPAWN"), "{err}");
    }

    #[tokio::test]
    async fn probing_a_closed_port_is_false_not_an_error() {
        assert!(!probe("http://127.0.0.1:1").await);
    }

    #[test]
    fn the_log_sits_next_to_the_catalog() {
        let log = log_path();
        assert!(log.ends_with("finderyd.log"), "{log}");
        assert!(
            !log.contains('\\'),
            "printed paths stay in one separator: {log}"
        );
    }
}
