//! Where things live on disk: the data directory, the buckets inside it, and
//! the configuration file a user owns.
//!
//! INTENT: split out of the configuration sections, which describe the knobs
//! (CONV-029). "Which value does this setting take" and "which file does it
//! end up in" are two responsibilities, and the second one is read by clients
//! that never load a configuration at all.

use std::path::{Path, PathBuf};

use crate::embedding::recipe::{Recipe, RecipeTag};

/// Directory holding every bucket but the default one.
pub fn buckets_dir(data_dir: &str) -> String {
    format!("{}/buckets", data_dir.trim_end_matches('/'))
}

/// Path of one bucket's database file.
///
/// INTENT: the default bucket keeps the flat `bucket.db` path it has always
/// had, so an existing install is not migrated by this feature and a single
/// bucket never grows a directory level. Named buckets live one level down,
/// where their name is the directory and nothing else may be.
pub fn bucket_path(data_dir: &str, name: &str) -> String {
    if name == crate::model::DEFAULT_BUCKET {
        format!("{}/bucket.db", data_dir.trim_end_matches('/'))
    } else {
        format!("{}/{name}/bucket.db", buckets_dir(data_dir))
    }
}

/// Where the bucket lives when nothing says otherwise.
///
/// INTENT: the default targets a daemon running natively on a developer
/// machine, because that is the setup with no bind mount, no path translation
/// and therefore no limit on which disk a project may live on. Compose
/// overrides it with `/data`.
pub fn default_data_dir() -> String {
    let home = |var: &str, suffix: &str| {
        std::env::var(var).ok().filter(|v| !v.is_empty()).map(|v| {
            let base = v.replace('\\', "/");
            format!("{}{suffix}", base.trim_end_matches('/'))
        })
    };
    home("LOCALAPPDATA", "/findery")
        .or_else(|| home("XDG_DATA_HOME", "/findery"))
        .or_else(|| home("HOME", "/.local/share/findery"))
        // Never `./.findery`: that directory is a project's manifest, and a
        // data directory named after it would put a bucket beside a manifest
        // that describes a different project.
        .unwrap_or_else(|| "./.findery-data".to_string())
}

/// The configuration file a user owns, next to their bucket.
///
/// INTENT: settings had only two homes, and neither belonged to the user. A
/// path like `/etc/findery/config.toml` means nothing on Windows and needs root
/// on Linux; environment variables live in a shell profile the daemon may never
/// read, and vanish from the next terminal. A file beside the bucket is
/// writable, per-user, survives a reboot, and is somewhere both the CLI and the
/// daemon can agree on without being told.
///
/// Naming it here rather than in either of them is the point: two definitions
/// of "where the settings live" is a setting that silently does nothing.
pub fn user_config_path() -> String {
    format!("{}/config.toml", data_dir_from_env())
}

/// Where the bucket lives for a process that has not loaded a configuration.
///
/// INTENT: clients need to *name* the data directory — in a diagnostic, in an
/// error pointing at the daemon's log — without opening it. Reading
/// `FINDERY_DATA_DIR` here is what keeps those messages pointing at the
/// directory the daemon will actually use; [`default_data_dir`] alone would
/// send the reader to a file that does not exist.
///
/// It is deliberately not the whole overlay: a configuration *file* can also
/// move the data directory, and only the daemon reads that. Clients that need
/// certainty ask the daemon.
pub fn data_dir_from_env() -> String {
    std::env::var("FINDERY_DATA_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.replace('\\', "/").trim_end_matches('/').to_string())
        .unwrap_or_else(default_data_dir)
}

/// The file holding one recipe's member of the bucket whose base path is
/// `base` (`dir/stem.db`): `dir/{stem}.{slug}-{tag}.db`.
///
/// INTENT: one bucket holds one file per vector space, so switching model never
/// has to refuse, rename or overwrite anything — the other model's vectors stay
/// beside it, and switching back costs nothing. The slug is for the human
/// reading a directory listing; the tag is what [`parse_member_name`] reads
/// back. The slug never contains a `.`, which is what keeps
/// `bucket.foo.x-abcdefgh.db` from ever being read as a member of `bucket`.
///
/// The only place a member's file name is spelled.
pub fn bucket_member_path(base: &Path, recipe: &Recipe) -> PathBuf {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("bucket");
    let name = format!("{stem}.{}-{}.db", recipe.slug(), recipe.tag());
    match base.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// The recipe tag of `file_name` when it names a member of the bucket whose
/// base file stem is `stem` — `{stem}.{middle}-{tag}.db`, the middle holding no
/// `.` — and `None` for anything else, the base file and its `-wal`/`-shm`
/// included.
pub fn parse_member_name(stem: &str, file_name: &str) -> Option<RecipeTag> {
    let rest = file_name.strip_prefix(stem)?.strip_prefix('.')?;
    let body = rest.strip_suffix(".db")?;
    if body.contains('.') {
        return None;
    }
    let (slug, tag) = body.rsplit_once('-')?;
    if slug.is_empty() {
        return None;
    }
    RecipeTag::parse(tag).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(raw: &str) -> Recipe {
        Recipe::parse(raw).expect("recipe")
    }

    #[test]
    fn a_member_sits_beside_its_base_file_and_reads_back_to_its_tag() {
        let r = recipe("harrier-oss-v1-0.6b:1024");
        let path = bucket_member_path(Path::new("/p/.findery/bucket.db"), &r);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name");
        assert_eq!(path.parent(), Some(Path::new("/p/.findery")));
        assert_eq!(name, format!("bucket.harrier-oss-v1-0-6b-{}.db", r.tag()));
        assert_eq!(parse_member_name("bucket", name), Some(r.tag()));
    }

    #[test]
    fn a_bare_base_name_still_gets_a_member() {
        let r = recipe("mock:256");
        let path = bucket_member_path(Path::new("bucket.db"), &r);
        assert_eq!(path, PathBuf::from(format!("bucket.mock-{}.db", r.tag())));
    }

    #[test]
    fn only_a_members_own_name_parses_as_one() {
        let tag = recipe("mock:256").tag();
        let member = format!("bucket.mock-{tag}.db");
        assert_eq!(parse_member_name("bucket", &member), Some(tag));
        // Another family's member, whose stem happens to extend ours.
        let nested = format!("bucket.foo.x-{tag}.db");
        assert_eq!(parse_member_name("bucket", &nested), None);
        assert_eq!(parse_member_name("bucket.foo", &nested), Some(tag));
        for other in [
            "bucket.db".to_string(),
            "bucket.db-wal".to_string(),
            format!("bucket.mock-{tag}.db-wal"),
            format!("bucket.mock-{tag}.db-shm"),
            format!("bucket.-{tag}.db"),
            "bucket.mock-ABCDEFGH.db".to_string(),
            "bucket.mock-abcdefg.db".to_string(),
            "bucket.mock.db".to_string(),
            format!("buckets.mock-{tag}.db"),
            format!("other.mock-{tag}.db"),
        ] {
            assert_eq!(parse_member_name("bucket", &other), None, "{other}");
        }
    }
}
