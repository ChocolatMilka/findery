//! Finding and opening every bucket family on disk.
//!
//! INTENT: a bucket is its files, so the daemon's map of them is rebuilt from
//! the filesystem — at boot, and whenever a family that was closed (all its
//! projects detached) is attached again or removed. The same three steps serve
//! all of them: **scan** a family's members and peek at the recipe each records
//! without opening it for real; **remember** the width each identity was built
//! at, which is what lets a profile whose runtime is down still boot; **open**
//! the members, keyed by the tag the resolver will look them up by.
//!
//! One file that cannot be read never stops the others: it is reported, the
//! way a bucket that silently failed to open would otherwise be a project that
//! silently vanished from every listing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use findery_core::config::{StoreConfig, bucket_path};
use findery_core::embedding::recipe::{Recipe, RecipeTag};
use findery_core::ids::ProjectId;
use findery_core::model::DEFAULT_BUCKET;
use findery_core::{Error, Result};

use findery_store::files::registry::Registry;
use findery_store::{Bucket, FamilyPath, Member};

use crate::models::EmbedderPool;
use crate::storage::blocking::blocking;
use crate::storage::families::{BucketFamily, BucketHandle, shown};
use crate::storage::member::MemberSpec;

/// One member file and the recipe it records, read without opening it.
pub(crate) struct Peeked {
    pub member: Member,
    pub recipe: Result<Option<String>>,
}

/// One family's files, peeked.
pub(crate) struct FamilyScan {
    pub name: String,
    pub base: FamilyPath,
    pub members: Vec<Peeked>,
}

/// Where every family this data directory knows lives: the default one, each
/// `buckets/<name>/`, and each registry entry — minus `skip`, the families a
/// pending removal could not finish. Failures to list are reported, not fatal.
pub(crate) fn locate(
    cfg: &StoreConfig,
    registry: &Registry,
    skip: &HashSet<String>,
) -> (BTreeMap<String, FamilyPath>, Vec<String>) {
    let mut broken = Vec::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    match findery_store::list_families(cfg) {
        Ok(found) => {
            for name in found {
                let derived = bucket_path(&cfg.data_dir, &name);
                names.insert(name, derived);
            }
        }
        Err(e) => broken.push(format!("the bucket directory could not be listed: {e}")),
    }
    // The default bucket always exists, so a fresh install has somewhere to put
    // its first project without a create step — even before it has a file.
    names
        .entry(DEFAULT_BUCKET.to_string())
        .or_insert_with(|| bucket_path(&cfg.data_dir, DEFAULT_BUCKET));
    for (name, file) in &registry.buckets {
        // A name found on disk keeps the path its name derives, which is what
        // makes a lost registry survivable; the registry covers the rest.
        names.entry(name.clone()).or_insert_with(|| file.clone());
    }
    let mut out = BTreeMap::new();
    for (name, file) in names {
        if skip.contains(&name) {
            continue;
        }
        match FamilyPath::from_base(Path::new(&file)) {
            Ok(base) => {
                out.insert(name, base);
            }
            Err(e) => broken.push(format!("bucket `{name}` at {file}: {}", e.detail())),
        }
    }
    (out, broken)
}

/// Every located family, scanned — default first, then by name: the order a
/// tie between two homes takes.
pub(crate) async fn scan_all(
    cfg: &StoreConfig,
    registry: &Registry,
    skip: &HashSet<String>,
    broken: &mut Vec<String>,
) -> Vec<FamilyScan> {
    let (located, unlocated) = locate(cfg, registry, skip);
    broken.extend(unlocated);
    let mut scans = Vec::new();
    for (name, base) in &located {
        match scan_async(cfg, name, base).await {
            Ok(scan) => scans.push(scan),
            Err(e) => broken.push(format!(
                "bucket `{name}` could not be listed: {}",
                e.detail()
            )),
        }
    }
    scans.sort_by_key(|s| (s.name != DEFAULT_BUCKET, s.name.clone()));
    scans
}

/// Lists a family's members and peeks at each one's recipe, on the blocking
/// pool: SQLite and the filesystem, with a short busy timeout per file.
pub(crate) async fn scan_async(
    cfg: &StoreConfig,
    name: &str,
    base: &FamilyPath,
) -> Result<FamilyScan> {
    let (name, base, busy) = (name.to_string(), base.clone(), cfg.peek_busy_timeout());
    blocking(move || {
        let members = findery_store::scan(&base)?
            .into_iter()
            .map(|member| {
                let recipe = findery_store::read_recipe_no_create(&member.path, busy);
                Peeked { member, recipe }
            })
            .collect();
        Ok(FamilyScan {
            name,
            base,
            members,
        })
    })
    .await
}

/// The width each recipe identity was built at, across every member peeked.
///
/// The width does not depend on the document template, so a `|doc=` recipe of
/// the same identity remembers it just as well. The first one found wins: two
/// widths of one identity is two buckets of one model, and either is a width
/// the model really served.
pub(crate) fn remembered_dims(scans: &[FamilyScan]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for peeked in scans.iter().flat_map(|s| &s.members) {
        if let Ok(Some(raw)) = &peeked.recipe
            && let Ok(recipe) = Recipe::parse(raw)
        {
            out.entry(recipe.identity().to_string())
                .or_insert(recipe.dim());
        }
    }
    out
}

/// What to do with one peeked file.
enum Action {
    /// Nothing: shadowed by a tagged member of its recipe, claimed later by
    /// its model's first write, or a member whose removal is pending.
    Skip,
    /// A base file recording no recipe: a source of rows, claimed by the first
    /// write.
    Unclaimed,
    Open(MemberSpec),
    Broken(String),
}

/// How a scanned family is opened.
pub(crate) struct Opener<'a> {
    pub cfg: &'a StoreConfig,
    /// What a member's tag promises, when a profile names it; `None` reads the
    /// recipe the file records — enough for a removal, which embeds nothing.
    pub pool: Option<&'a EmbedderPool>,
    /// Member files whose removal is pending: never opened again.
    pub removing: &'a BTreeSet<String>,
}

impl Opener<'_> {
    /// Opens a scanned family's members. Returns the family and what could not
    /// be opened, one line each.
    pub(crate) async fn open(&self, scan: FamilyScan) -> (BucketFamily, Vec<String>) {
        let family = BucketFamily::new(self.cfg, &scan.name, scan.base.clone());
        let mut broken = Vec::new();
        // A tagged member wins over the legacy file holding the same recipe:
        // adoption found the target taken and left the legacy file for the user.
        let tagged: HashSet<_> = scan.members.iter().filter_map(|p| p.member.tag).collect();
        for peeked in &scan.members {
            let path = &peeked.member.path;
            let opened = match self.classify(peeked, &tagged) {
                Action::Skip => Ok(()),
                Action::Unclaimed => open_unclaimed(&family, path).await,
                Action::Open(spec) => open_member(self.cfg, &family, path, spec).await,
                Action::Broken(why) => Err(Error::Invalid(why)),
            };
            if let Err(e) = opened {
                broken.push(format!(
                    "bucket `{}` could not be opened: {} ({})",
                    scan.name,
                    path.display(),
                    e.detail()
                ));
            }
        }
        (family, broken)
    }

    fn classify(&self, peeked: &Peeked, tagged: &HashSet<RecipeTag>) -> Action {
        let raw = match &peeked.recipe {
            Ok(raw) => raw.as_deref(),
            Err(e) => return Action::Broken(e.detail()),
        };
        if self.removing.contains(&shown(&peeked.member.path)) {
            return Action::Skip;
        }
        match (peeked.member.tag, raw) {
            (None, None) => Action::Unclaimed,
            (None, Some(raw)) => match Recipe::parse(raw) {
                Ok(recipe) if tagged.contains(&recipe.tag()) => Action::Skip,
                Ok(recipe) => Action::Open(MemberSpec {
                    tag: recipe.tag(),
                    recipe,
                    legacy: true,
                }),
                Err(e) => Action::Broken(format!(
                    "it records `{raw}`, which names no vector space ({})",
                    e.detail()
                )),
            },
            // A tagged file that records nothing: the first write of its model
            // claims it, through the same path that would have created it.
            (Some(_), None) => Action::Skip,
            (Some(tag), Some(raw)) => match self.named_for(&tag, raw) {
                Some(recipe) => Action::Open(MemberSpec {
                    tag,
                    recipe,
                    legacy: false,
                }),
                None => Action::Broken(format!("it records `{raw}`, which names no vector space")),
            },
        }
    }

    /// What a tagged file is expected to hold: the recipe a profile resolves
    /// its tag to, else the one it records.
    fn named_for(&self, tag: &RecipeTag, raw: &str) -> Option<Recipe> {
        self.pool
            .and_then(|pool| pool.recipe_for_tag(tag))
            .or_else(|| Recipe::parse(raw).ok())
    }
}

/// Opens one member file and adds it to its family under its tag.
async fn open_member(
    cfg: &StoreConfig,
    family: &BucketFamily,
    path: &Path,
    spec: MemberSpec,
) -> Result<()> {
    let bucket = open_existing(path, &family.name).await?;
    let handle = BucketHandle::open(cfg, family, bucket, spec).await?;
    family.insert(handle);
    Ok(())
}

/// Opens the base file that records no recipe, as a source of rows.
async fn open_unclaimed(family: &BucketFamily, path: &Path) -> Result<()> {
    family.set_unclaimed(open_existing(path, &family.name).await?);
    Ok(())
}

async fn open_existing(path: &Path, name: &str) -> Result<Bucket> {
    let (path, name) = (path.to_path_buf(), name.to_string());
    blocking(move || Bucket::open_existing(&path, &name)).await
}

/// The daemon's families once boot opened them, and where each project lives.
pub(crate) struct Opened {
    pub families: BTreeMap<String, Arc<BucketFamily>>,
    pub homes: HashMap<ProjectId, String>,
    pub notices: Vec<String>,
    pub broken: Vec<String>,
}

/// Opens every scanned family — but the ones whose projects are all
/// detached — and picks each attached project's home.
pub(crate) async fn open_all(
    cfg: &StoreConfig,
    pool: &EmbedderPool,
    registry: &Registry,
    scans: Vec<FamilyScan>,
) -> Opened {
    let opener = Opener {
        cfg,
        pool: Some(pool),
        removing: &registry.removing_members,
    };
    let detached: HashSet<&str> = registry.detached.keys().map(String::as_str).collect();
    let mut opened = Opened {
        families: BTreeMap::new(),
        homes: HashMap::new(),
        notices: Vec::new(),
        broken: Vec::new(),
    };
    let mut candidates = Vec::new();
    for (order, scan) in scans.into_iter().enumerate() {
        let (family, failed) = opener.open(scan).await;
        opened.broken.extend(failed);
        let Some(attached) = attached_ids(&family, &detached, &mut opened.broken).await else {
            continue;
        };
        for id in attached {
            match family.freshest_row(&id).await {
                Ok(Some(row)) => candidates.push(Candidate {
                    id,
                    updated_at: row.updated_at,
                    order,
                    family: family.name.clone(),
                }),
                Ok(None) => {}
                Err(e) => opened.broken.push(format!(
                    "project `{id}` in bucket `{}` could not be read: {}",
                    family.name,
                    e.detail()
                )),
            }
        }
        opened
            .families
            .insert(family.name.clone(), Arc::new(family));
    }
    let (homes, duplicates) = pick_homes(candidates);
    opened.homes = homes;
    opened.notices = duplicates;
    opened
}

/// The attached projects of a just-opened family, or `None` when the family
/// stays closed: unreadable, or every project in it detached.
async fn attached_ids(
    family: &BucketFamily,
    detached: &HashSet<&str>,
    broken: &mut Vec<String>,
) -> Option<Vec<ProjectId>> {
    let ids = match family.project_ids().await {
        Ok(ids) => ids,
        Err(e) => {
            broken.push(format!(
                "bucket `{}` could not be read: {}",
                family.name,
                e.detail()
            ));
            return None;
        }
    };
    let attached: Vec<ProjectId> = ids
        .iter()
        .filter(|id| !detached.contains(id.as_str()))
        .cloned()
        .collect();
    // A family whose every project is detached stays closed: its files were
    // released on purpose, and reopening them would lock them again.
    let released = !ids.is_empty() && attached.is_empty() && family.name != DEFAULT_BUCKET;
    if !released {
        return Some(attached);
    }
    if let Err(e) = family.close().await {
        broken.push(format!(
            "bucket `{}` could not be closed: {}",
            family.name,
            e.detail()
        ));
    }
    None
}

/// One project row found at boot, and where.
pub(crate) struct Candidate {
    pub id: ProjectId,
    pub updated_at: i64,
    /// The family's position in boot order: default first, then by name.
    pub order: usize,
    pub family: String,
}

/// Which family each project calls home.
///
/// INTENT: a project id in two families is a hand-edited or half-moved state
/// the daemon's own writes cannot create. The freshest row wins — the one a
/// `register` wrote last — with boot order breaking a tie, and the duplicate is
/// reported: the old map took whichever family it met last, silently, which is
/// a project whose search answers from a stale index for no visible reason.
pub(crate) fn pick_homes(candidates: Vec<Candidate>) -> (HashMap<ProjectId, String>, Vec<String>) {
    let mut best: HashMap<ProjectId, Candidate> = HashMap::new();
    let mut seen: BTreeMap<ProjectId, Vec<String>> = BTreeMap::new();
    for candidate in candidates {
        let families = seen.entry(candidate.id.clone()).or_default();
        if !families.contains(&candidate.family) {
            families.push(candidate.family.clone());
        }
        let fresher = best.get(&candidate.id).is_none_or(|kept| {
            (candidate.updated_at, std::cmp::Reverse(candidate.order))
                > (kept.updated_at, std::cmp::Reverse(kept.order))
        });
        if fresher {
            best.insert(candidate.id.clone(), candidate);
        }
    }
    let mut notices = Vec::new();
    for (id, families) in seen {
        if families.len() > 1
            && let Some(home) = best.get(&id)
        {
            notices.push(format!(
                "project `{id}` has rows in buckets {}; serving `{}`, the most recently \
                 registered — `findery rm` the copy you no longer want",
                families
                    .iter()
                    .map(|f| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                home.family
            ));
        }
    }
    let homes = best
        .into_iter()
        .map(|(id, candidate)| (id, candidate.family))
        .collect();
    (homes, notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, updated_at: i64, order: usize, family: &str) -> Candidate {
        Candidate {
            id: ProjectId::new(id).expect("id"),
            updated_at,
            order,
            family: family.into(),
        }
    }

    /// The bug this replaces: the last family met won, whatever its row said.
    #[test]
    fn a_project_in_two_buckets_lives_in_the_freshest_and_says_so() {
        let (homes, notices) = pick_homes(vec![
            candidate("axle", 50, 0, "default"),
            candidate("axle", 90, 1, "alpha"),
            candidate("axle", 10, 2, "zeta"),
            candidate("solo", 5, 2, "zeta"),
        ]);
        let axle = ProjectId::new("axle").expect("id");
        assert_eq!(homes[&axle], "alpha", "freshest, not last");
        assert_eq!(homes[&ProjectId::new("solo").expect("id")], "zeta");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("`axle`") && notices[0].contains("serving `alpha`"));
    }

    #[test]
    fn a_tie_goes_to_the_family_first_in_boot_order() {
        let (homes, _) = pick_homes(vec![
            candidate("axle", 50, 2, "zeta"),
            candidate("axle", 50, 0, "default"),
        ]);
        assert_eq!(homes[&ProjectId::new("axle").expect("id")], "default");
    }

    /// Two members of one family carrying the same project are one home, not a
    /// duplicate worth a notice.
    #[test]
    fn rows_in_two_members_of_one_family_are_not_a_duplicate() {
        let (_, notices) = pick_homes(vec![
            candidate("axle", 50, 0, "default"),
            candidate("axle", 60, 0, "default"),
        ]);
        assert!(notices.is_empty(), "{notices:?}");
    }
}
