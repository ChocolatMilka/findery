//! `findery config` — settings that live in the product, not in a shell.
//!
//! INTENT: every setting used to be an environment variable. That means a shell
//! profile the daemon may never read, a value that vanishes with the terminal,
//! and on Windows a `SetEnvironmentVariable` call plus a new session before
//! anything takes effect. Four exports to name a model is not configuration, it
//! is a workaround.
//!
//! This writes the file the daemon already reads, and it writes it the only way
//! that cannot leave a daemon refusing to boot: the whole document is parsed and
//! validated as a `Config` **before** it reaches the disk (CONV-023).
//!
//! The read half is [`connection`]: the daemon address and the token live in
//! that same file, so a client finds its daemon without a shell involved. It
//! touches no HTTP. The file is local and so is the edit; the daemon's only part
//! is being restarted so it reads it.

use findery_core::config::{Config, user_config_path};
use findery_core::{Error, Result};

/// Friendly names for the keys people actually set.
///
/// INTENT: `findery config set model …` is what someone types. Making them
/// learn `embedder.model` to change a model is the kind of tax that sends them
/// back to environment variables.
const ALIASES: &[(&str, &str)] = &[
    ("model", "embedder.model"),
    ("url", "embedder.base_url"),
    ("server", "server.url"),
    ("key", "embedder.api_key"),
    ("api-key", "embedder.api_key"),
    ("dim", "embedder.dim"),
    ("dims", "embedder.dim"),
    ("instruction", "embedder.query_instruction"),
    ("store", "store.backend"),
    ("data-dir", "store.data_dir"),
    ("bind", "server.bind"),
    ("token", "server.api_token"),
    ("extensions", "project_defaults.extensions"),
];

/// Keys whose value must never be echoed back.
const SECRETS: &[&str] = &[
    "embedder.api_key",
    "server.api_token",
    "store.qdrant.api_key",
];

/// Prints the path of the file settings are written to.
pub fn path(json: bool) -> Result<()> {
    let path = user_config_path();
    if json {
        return crate::render::json(&serde_json::json!({ "path": path }));
    }
    println!("{path}");
    Ok(())
}

/// The daemon address and token, as the settings file spells them.
///
/// INTENT: naming the daemon used to be an environment variable, which on
/// Windows costs a `setx` and a new session, and on any platform is invisible to
/// a process started from a shortcut. Reading it out of the same file the daemon
/// reads is what makes `findery config set server <url>` enough to point every
/// command on this machine at one daemon — and what lets the token be typed
/// once, next to the address it belongs to.
///
/// Either may be absent: an empty string is "not set", not a setting.
pub fn connection() -> Result<(Option<String>, Option<String>)> {
    let document = read(&user_config_path())?;
    let server = document.get("server").and_then(toml::Value::as_table);
    let setting = |key: &str| -> Option<String> {
        server?
            .get(key)?
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    Ok((setting("url"), setting("api_token")))
}

/// Prints the file as it stands, with secrets masked.
pub fn show(json: bool) -> Result<()> {
    let path = user_config_path();
    let document = read(&path)?;

    if json {
        // Round-tripping through the typed Config would print every default and
        // bury the handful of lines the user actually chose.
        let set: Vec<_> = flatten(&document)
            .into_iter()
            .map(|(k, v)| serde_json::json!({ "key": k, "value": mask(&k, &v) }))
            .collect();
        return crate::render::json(&serde_json::json!({ "path": path, "settings": set }));
    }

    let settings = flatten(&document);
    if settings.is_empty() {
        println!("{path}\n  (nothing set; every value is a default)");
        return Ok(());
    }
    println!("{path}");
    for (key, value) in settings {
        println!("  {key} = {}", mask(&key, &value));
    }
    Ok(())
}

/// Writes one setting, refusing anything the daemon would reject.
///
/// Returns the environment variable that overrides the key, when one is set:
/// the caller is the only place that still knows how to warn about it, and the
/// resolved key is the only thing that can find it.
pub fn set(key: &str, value: &str, json: bool) -> Result<Option<&'static str>> {
    let key = resolve(key);
    let path = user_config_path();
    let mut document = read(&path)?;

    assign(&mut document, &key, parse_scalar(&key, value))?;
    verify(&document)?;
    write(&path, &document)?;

    if json {
        crate::render::json(&serde_json::json!({
            "path": path, "key": key, "value": mask(&key, &shown(&document, &key)),
        }))?;
    } else {
        println!("{key} = {}", mask(&key, &shown(&document, &key)));
    }
    Ok(shadowed(&key))
}

/// Removes one setting, returning it to its default.
///
/// Like [`set`], reports an environment variable that would mask the change.
pub fn unset(key: &str, json: bool) -> Result<Option<&'static str>> {
    let key = resolve(key);
    let path = user_config_path();
    let mut document = read(&path)?;

    let existed = remove(&mut document, &key);
    verify(&document)?;
    write(&path, &document)?;

    if json {
        crate::render::json(&serde_json::json!({ "path": path, "key": key, "removed": existed }))?;
    } else if existed {
        println!("{key} is back to its default");
    } else {
        println!("{key} was not set");
    }
    Ok(shadowed(&key))
}

/// Expands an alias, or passes a dotted path through untouched.
fn resolve(key: &str) -> String {
    let key = key.trim();
    ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(key))
        .map_or_else(|| key.to_string(), |(_, full)| (*full).to_string())
}

/// Reads the document, treating "no file yet" as "nothing set".
fn read(path: &str) -> Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(src) => src
            .parse::<toml::Table>()
            .map_err(|e| Error::Config(format!("`{path}` is not valid TOML: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(Error::Config(format!("cannot read `{path}`: {e}"))),
    }
}

/// Rejects a document the daemon would refuse, before it reaches the disk.
///
/// INTENT: a bad value written to the file is a daemon that will not boot, and
/// the only way back is editing the file by hand — exactly what this command
/// exists to avoid. `deny_unknown_fields` on `Config` also makes this catch a
/// misspelled key, which a plain TOML write never would.
fn verify(document: &toml::Table) -> Result<()> {
    let text = toml::to_string_pretty(document)
        .map_err(|e| Error::Config(format!("cannot serialize the settings: {e}")))?;
    Config::from_toml_str(&text)?.validate()
}

fn write(path: &str, document: &toml::Table) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Config(format!("cannot create `{}`: {e}", parent.display())))?;
    }
    let text = toml::to_string_pretty(document)
        .map_err(|e| Error::Config(format!("cannot serialize the settings: {e}")))?;
    let banner = "# findery settings. Written by `findery config set`.\n\
                  # Environment variables still win over anything here.\n\n";
    std::fs::write(path, format!("{banner}{text}"))
        .map_err(|e| Error::Config(format!("cannot write `{path}`: {e}")))
}

/// Keys whose value is a list, so `rs,md` means what the user thinks it means.
///
/// Splitting on a comma everywhere would wreck `query_instruction`, whose whole
/// job is to be an English sentence.
const LISTS: &[&str] = &["project_defaults.extensions", "project_defaults.exclude"];

/// Turns what was typed into the TOML value the schema expects.
///
/// `dim 2560` has to become an integer and `send_dimensions true` a boolean, or
/// the document parses into a `Config` that rejects them. There is no scalar
/// parser in `toml` — `Value::from_str` wants a whole document — so the value is
/// parsed as the right-hand side of one, and anything that is not valid TOML on
/// its own is what it looks like: a string.
fn parse_scalar(key: &str, raw: &str) -> toml::Value {
    if LISTS.contains(&key) && !raw.trim_start().starts_with('[') {
        return toml::Value::Array(
            raw.split(',')
                .map(|item| toml::Value::String(item.trim().to_string()))
                .filter(|item| item.as_str().is_some_and(|s| !s.is_empty()))
                .collect(),
        );
    }
    format!("v = {raw}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|table| table.get("v").cloned())
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

/// Writes `a.b.c = value`, creating the tables on the way.
fn assign(document: &mut toml::Table, key: &str, value: toml::Value) -> Result<()> {
    let (parents, leaf) = split(key)?;
    let mut table = document;
    for part in parents {
        let entry = table
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        table = entry.as_table_mut().ok_or_else(|| {
            Error::Invalid(format!(
                "`{part}` holds a value, so `{key}` cannot go under it"
            ))
        })?;
    }
    table.insert(leaf.to_string(), value);
    Ok(())
}

/// Removes `a.b.c`, and any table it leaves empty.
fn remove(document: &mut toml::Table, key: &str) -> bool {
    let Ok((parents, leaf)) = split(key) else {
        return false;
    };
    let mut table = &mut *document;
    for part in &parents {
        match table.get_mut(*part).and_then(toml::Value::as_table_mut) {
            Some(next) => table = next,
            None => return false,
        }
    }
    let existed = table.remove(leaf).is_some();
    // A `[embedder]` header with nothing under it reads as a setting that is
    // still there.
    prune(document);
    existed
}

fn prune(table: &mut toml::Table) {
    let empties: Vec<String> = table
        .iter_mut()
        .filter_map(|(k, v)| {
            let inner = v.as_table_mut()?;
            prune(inner);
            inner.is_empty().then(|| k.clone())
        })
        .collect();
    for key in empties {
        table.remove(&key);
    }
}

fn split(key: &str) -> Result<(Vec<&str>, &str)> {
    let mut parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    let leaf = parts
        .pop()
        .ok_or_else(|| Error::Invalid("a key cannot be empty".into()))?;
    Ok((parts, leaf))
}

/// Every leaf in the document, as dotted keys.
fn flatten(table: &toml::Table) -> Vec<(String, String)> {
    fn walk(table: &toml::Table, prefix: &str, out: &mut Vec<(String, String)>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value.as_table() {
                Some(inner) => walk(inner, &path, out),
                None => out.push((path, render_value(value))),
            }
        }
    }
    let mut out = Vec::new();
    walk(table, "", &mut out);
    out
}

fn render_value(value: &toml::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToString::to_string)
}

fn shown(document: &toml::Table, key: &str) -> String {
    flatten(document)
        .into_iter()
        .find(|(k, _)| k == key)
        .map_or_else(String::new, |(_, v)| v)
}

/// Hides a secret's body while still proving it is set.
fn mask(key: &str, value: &str) -> String {
    if !SECRETS.contains(&key) || value.is_empty() {
        return value.to_string();
    }
    let tail: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

/// The environment variable currently overriding a key, if any.
///
/// INTENT: the overlay is deliberate — one container image has to serve every
/// deployment — but it means a `config set` can appear to do nothing. Naming the
/// variable at the moment of the write is the difference between a rule and a
/// trap.
fn shadowed(key: &str) -> Option<&'static str> {
    env_var_for(key).filter(|var| std::env::var(var).is_ok_and(|v| !v.is_empty()))
}

/// The environment variable that would win over a key, when there is one.
fn env_var_for(key: &str) -> Option<&'static str> {
    Some(match key {
        "embedder.model" => "FINDERY_EMBED_MODEL",
        "embedder.base_url" => "FINDERY_EMBED_BASE_URL",
        "embedder.api_key" => "FINDERY_EMBED_API_KEY",
        "embedder.dim" => "FINDERY_EMBED_DIM",
        "embedder.query_instruction" => "FINDERY_EMBED_QUERY_INSTRUCTION",
        "embedder.backend" => "FINDERY_EMBED_BACKEND",
        "store.backend" => "FINDERY_STORE_BACKEND",
        "store.data_dir" => "FINDERY_DATA_DIR",
        "server.bind" => "FINDERY_BIND",
        "server.api_token" => "FINDERY_API_TOKEN",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_names_reach_the_real_keys() {
        assert_eq!(resolve("model"), "embedder.model");
        assert_eq!(resolve("DIM"), "embedder.dim");
        // A dotted key nobody aliased still has to work.
        assert_eq!(resolve("search.rrf_k"), "search.rrf_k");
    }

    #[test]
    fn a_width_is_written_as_a_number_not_a_string() {
        let mut doc = toml::Table::new();
        assign(
            &mut doc,
            "embedder.dim",
            parse_scalar("embedder.dim", "2560"),
        )
        .expect("assign");
        assert!(doc["embedder"]["dim"].is_integer(), "{doc}");

        assign(
            &mut doc,
            "embedder.send_dimensions",
            parse_scalar("embedder.send_dimensions", "true"),
        )
        .expect("assign");
        assert!(doc["embedder"]["send_dimensions"].is_bool(), "{doc}");

        // A model name is not a number and must survive as written.
        assign(
            &mut doc,
            "embedder.model",
            parse_scalar("embedder.model", "qwen3.7-text-embedding"),
        )
        .expect("assign");
        assert_eq!(
            doc["embedder"]["model"].as_str(),
            Some("qwen3.7-text-embedding")
        );
    }

    #[test]
    fn a_list_key_takes_a_comma_list_but_a_sentence_never_does() {
        let extensions = parse_scalar("project_defaults.extensions", "rs, md,toml");
        let items: Vec<&str> = extensions
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(items, ["rs", "md", "toml"]);

        // The instruction is an English sentence and its commas belong to it.
        let instruction = parse_scalar(
            "embedder.query_instruction",
            "Given a query, retrieve the code",
        );
        assert_eq!(
            instruction.as_str(),
            Some("Given a query, retrieve the code")
        );
    }

    #[test]
    fn a_value_the_daemon_would_refuse_never_reaches_the_disk() {
        let mut doc = toml::Table::new();
        assign(
            &mut doc,
            "embedder.batch_size",
            parse_scalar("embedder.batch_size", "0"),
        )
        .expect("assign");
        let err = verify(&doc).expect_err("batch_size 0 is refused by validate");
        assert_eq!(err.code(), "config");
    }

    #[test]
    fn a_misspelled_key_is_caught_rather_than_silently_stored() {
        // `deny_unknown_fields` is what turns a typo into an error instead of a
        // setting that does nothing forever.
        let mut doc = toml::Table::new();
        assign(
            &mut doc,
            "embedder.modle",
            parse_scalar("embedder.modle", "x"),
        )
        .expect("assign");
        assert!(verify(&doc).is_err(), "a typo must not be writable");
    }

    #[test]
    fn unsetting_removes_the_key_and_the_table_it_emptied() {
        let mut doc = toml::Table::new();
        assign(
            &mut doc,
            "embedder.model",
            parse_scalar("embedder.model", "m"),
        )
        .expect("assign");
        assert!(remove(&mut doc, "embedder.model"));
        assert!(
            doc.is_empty(),
            "an empty [embedder] still reads as a setting: {doc}"
        );
        assert!(
            !remove(&mut doc, "embedder.model"),
            "removing twice is not a lie"
        );
    }

    #[test]
    fn a_secret_proves_it_is_set_without_printing_itself() {
        let masked = mask("embedder.api_key", "sk-abcdef123456");
        assert!(masked.starts_with("****"), "{masked}");
        assert!(!masked.contains("abcdef"), "{masked}");
        assert!(masked.ends_with("3456"), "{masked}");
        // Everything else prints as written.
        assert_eq!(
            mask("embedder.model", "qwen3.7-text-embedding"),
            "qwen3.7-text-embedding"
        );
    }

    #[test]
    fn the_warning_names_the_variable_the_user_typed_a_short_name_for() {
        // `config set model …` resolves to `embedder.model`; looking the
        // variable up under the short name would find nothing and leave the
        // warning unable to say what to unset.
        assert_eq!(
            env_var_for(&resolve("model")),
            Some("FINDERY_EMBED_MODEL"),
            "the alias has to be resolved before the lookup"
        );
        assert_eq!(env_var_for("model"), None);
        assert_eq!(env_var_for("search.rrf_k"), None);
    }
}
