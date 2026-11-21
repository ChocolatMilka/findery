//! What a project is, and where it lives.
//!
//! Split from `lib.rs`: registering a project decides everything about it at
//! once — how it is fed, which bucket holds its rows, which model it is pinned
//! to, what the walker must skip — and that reads better in one piece than
//! interleaved with search and export.

mod documents;
pub(crate) mod spec;
mod status;

use std::path::Path;
use std::sync::Arc;

use findery_core::api::RegisterProject;
use findery_core::embedding::recipe::Recipe;
use findery_core::ids::{ModelAlias, ProjectId};
use findery_core::manifest::ResolvedBucket;
use findery_core::model::Project;
use findery_core::support::clock::now_unix;
use findery_core::{Error, Result};

use findery_store::files::registry::{DetachedEntry, Registry};

use crate::Indexer;
use crate::projects::spec::{Decided, basename, exclusions_for, merged_spec};
use crate::storage::blocking::{read_registry, update_registry};
use crate::storage::families::BucketFamily;

/// Who a registration is about, decided before anything is written.
struct Identity {
    id: ProjectId,
    /// The root as the daemon sees it.
    root: String,
    /// True when a client walks the project and pushes it.
    push: bool,
    /// The root's last segment: the name when the request gives none.
    derived_name: String,
}

/// The bucket a registration lands in, open.
struct Target {
    resolved: ResolvedBucket,
    family: Arc<BucketFamily>,
}

impl Indexer {
    /// Registers a project, or updates it when the id already exists — or
    /// attaches it again when it was detached, which is instant: its files are
    /// where `detach` left them, and they are simply reopened.
    ///
    /// An update MERGES: whatever the request leaves absent keeps the value the
    /// project already has, bucket included, and only an explicitly empty value
    /// clears one (see [`RegisterProject`]).
    ///
    /// The bucket is chosen here and only here. Naming one the project does not
    /// currently live in *moves* it — a project is in exactly one bucket, and
    /// that is what makes `findery init --bucket work` a move rather than a
    /// silent duplicate. The row is written into EVERY member of the bucket, so
    /// scopes, excludes and the pin never diverge between models. The project
    /// is held still meanwhile: a sync of it waits, and this waits for one.
    pub async fn register(&self, req: RegisterProject) -> Result<Project> {
        // Before any side effect: a pin no profile defines is refused here,
        // naming the ones that exist.
        let pin = self.checked_request(&req)?;
        let identity = self.resolve_identity(&req).await?;
        let _still = self.hold_still(&identity.id).await?;
        let registry = read_registry(&self.cfg.store).await?;
        let attached = self.attached_row(&identity.id).await?;
        let detached = registry.detached_entry(&identity.id);
        let target = self
            .target_family(&req, &identity, attached.as_ref(), detached)
            .await?;
        // The project as it stands — attached, or detached in this family —
        // supplies whatever the request leaves unsaid.
        let previous = match attached {
            Some(project) => Some(project),
            None => target.family.freshest_row(&identity.id).await?,
        };
        let mut project = self.compose(req, identity, pin, &target, previous);
        let alias = self.pool.resolve(project.spec.model.as_ref())?;
        let recipe = self.pool.write_recipe(&alias).await?;
        self.write_registration(&target.family, &recipe, &project)
            .await?;
        self.finish_attach(&project, &registry, &target.family, &recipe)
            .await?;
        project.model = Some(alias);
        Ok(project)
    }

    /// Validates what needs no lookup, and returns the pin it asks for.
    fn checked_request(&self, req: &RegisterProject) -> Result<Option<Option<ModelAlias>>> {
        if req.root.trim().is_empty() {
            return Err(Error::Invalid("root must not be empty".into()));
        }
        let pin = self.requested_pin(req.model.as_deref())?;
        Ok(pin)
    }

    /// The project's id, its root as the daemon sees it, and how it is fed.
    async fn resolve_identity(&self, req: &RegisterProject) -> Result<Identity> {
        let root = self.mapper.to_container(&req.root);
        // Whether the daemon can read the directory decides HOW the project
        // gets fed, not whether it can be indexed. Unreadable means the client
        // walks it and pushes the contents — a deployment fact, not an error.
        let push = req.push.unwrap_or_else(|| !Path::new(&root).is_dir());
        let derived_name = basename(&root).unwrap_or_else(|| "project".into());
        // No id: the project already registered at this root, else one derived
        // from the name — registering a root again is never a second project.
        let id = match &req.id {
            Some(raw) => ProjectId::new(raw.clone())?,
            None => match self.project_at(&root).await? {
                Some(existing) => existing,
                None => ProjectId::slugify(req.name.as_deref().unwrap_or(&derived_name))?,
            },
        };
        Ok(Identity {
            id,
            root,
            push,
            derived_name,
        })
    }

    /// The project as attached right now; `None` when it is not attached.
    ///
    /// INTENT: only "not attached" is `None`. A row that cannot be read — a
    /// closed member, a SQLite failure — is an error: read as absent, it reset
    /// the spec to the defaults and moved the project out of its real home.
    async fn attached_row(&self, id: &ProjectId) -> Result<Option<Project>> {
        match self.get_project(id).await {
            Ok(project) => Ok(Some(project)),
            Err(Error::NotFound(_)) if self.home_of(id).is_err() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The bucket a registration lands in, opened.
    async fn target_family(
        &self,
        req: &RegisterProject,
        identity: &Identity,
        attached: Option<&Project>,
        detached: Option<&DetachedEntry>,
    ) -> Result<Target> {
        let resolved = self.bucket_for(req, identity, attached, detached)?;
        if resolved.substituted {
            tracing::warn!(
                project = %identity.id,
                using = %resolved.path,
                "the requested bucket is not reachable; using one beside the data directory"
            );
        }
        let family = self
            .ensure_bucket_at(&resolved.name, Some(&resolved.path))
            .await?;
        Ok(Target { resolved, family })
    }

    /// Which bucket a registration lands in: the one it names, else the one
    /// the project lives in — attached, or detached into — else the default
    /// resolution, a bucket inside the project.
    fn bucket_for(
        &self,
        req: &RegisterProject,
        identity: &Identity,
        attached: Option<&Project>,
        detached: Option<&DetachedEntry>,
    ) -> Result<ResolvedBucket> {
        if req.bucket.is_none() {
            if let Some(current) = attached {
                let family = self.family(&current.bucket)?;
                return Ok(ResolvedBucket {
                    name: current.bucket.clone(),
                    path: family.base.base().display().to_string(),
                    substituted: false,
                });
            }
            // A client that names a root alone — the MCP bridge — reattaches
            // the files `detach` left, never a fresh bucket beside them.
            if let Some(entry) = detached {
                return Ok(ResolvedBucket {
                    name: entry.family.clone(),
                    path: entry.base.clone(),
                    substituted: false,
                });
            }
        }
        // The daemon resolves rather than the client because it owns the data
        // directory and the mounts, and because it is the only side that knows
        // whether it can write inside this project at all.
        req.bucket.clone().unwrap_or_default().resolve(
            &self.cfg.store,
            &identity.id,
            &identity.root,
            !identity.push,
        )
    }

    /// The project row a registration writes.
    fn compose(
        &self,
        req: RegisterProject,
        identity: Identity,
        pin: Option<Option<ModelAlias>>,
        target: &Target,
        previous: Option<Project>,
    ) -> Project {
        let host_root = req.root.clone();
        let name = req
            .name
            .clone()
            .or_else(|| previous.as_ref().map(|p| p.name.clone()))
            .unwrap_or(identity.derived_name);
        let decided = Decided {
            root: identity.root,
            push: identity.push,
            pin,
        };
        let defaults = &self.cfg.project_defaults;
        let mut spec = merged_spec(defaults, req, decided, previous.as_ref().map(|p| &p.spec));
        // A bucket can live inside the project it indexes, and indexing the
        // index is a write loop: every sync would see a file it had just written.
        for pattern in exclusions_for(&target.resolved.path, &spec.root) {
            if !spec.exclude.contains(&pattern) {
                spec.exclude.push(pattern);
            }
        }
        Project {
            // A pushed project has no daemon-side path to map back from.
            host_root: if identity.push {
                host_root
            } else {
                self.mapper.to_host(&spec.root)
            },
            id: identity.id,
            name,
            bucket: target.resolved.name.clone(),
            model: None,
            spec,
            created_at: previous.map(|p| p.created_at).unwrap_or_else(now_unix),
            updated_at: now_unix(),
        }
    }

    /// Writes the row into the target family, every file of it, and warms its
    /// store — or refuses before anything is written, when the model's member
    /// holds another embedding space.
    async fn write_registration(
        &self,
        family: &BucketFamily,
        recipe: &Recipe,
        project: &Project,
    ) -> Result<()> {
        if let Some(member) = family.member(&recipe.tag()) {
            member.ensure_serves(recipe)?;
        }
        let member = self.writable_member(family, recipe, project).await?;
        member.ensure_serves(recipe)?;
        member
            .store
            .ensure_project(&project.id, recipe.dim())
            .await?;
        family.put_row_everywhere(project).await?;
        Ok(())
    }

    /// Makes the written row the project's home, then leaves the bucket it
    /// lived in — last, so a registration that failed on the way has moved
    /// nothing.
    async fn finish_attach(
        &self,
        project: &Project,
        registry: &Registry,
        family: &BucketFamily,
        recipe: &Recipe,
    ) -> Result<()> {
        let id = &project.id;
        if registry.detached_entry(id).is_some() {
            let key = id.to_string();
            update_registry(&self.cfg.store, move |r| r.detached.remove(&key)).await?;
            tracing::info!(project = %id, bucket = %project.bucket, "attached again");
        }
        let left = self.home_of(id).ok().filter(|home| *home != project.bucket);
        self.remember_home(id, &project.bucket);
        let Some(old) = left else {
            return Ok(());
        };
        if let Err(e) = self.leave_family(id, &old).await {
            // The project lives in its new bucket either way; what stays behind
            // is reported again at the next boot, as a duplicate row.
            tracing::warn!(project = %id, bucket = %old, error = %e, "the old bucket kept a copy");
        }
        // Two members of one recipe share a Qdrant collection name across
        // buckets; leaving the old one may have dropped it.
        let member = self.writable_member(family, recipe, project).await?;
        member.store.ensure_project(id, recipe.dim()).await
    }

    /// Takes a moved project's vectors and rows out of the family it left:
    /// built for that bucket's stores, they would keep costing for an index
    /// nothing points at.
    async fn leave_family(&self, id: &ProjectId, old: &str) -> Result<()> {
        let family = self.family(old)?;
        crate::lifecycle::removal::drop_rows_everywhere(&family, std::slice::from_ref(id)).await
    }

    /// The attached project whose root is `root`, compared as paths — without
    /// case on Windows, whose filesystem ignores it, and with it elsewhere,
    /// where `App` and `app` are two projects.
    async fn project_at(&self, root: &str) -> Result<Option<ProjectId>> {
        let wanted = findery_core::support::path_map::normalize(root);
        let same = |other: &str| {
            if cfg!(windows) {
                other.eq_ignore_ascii_case(&wanted)
            } else {
                other == wanted
            }
        };
        let found = self
            .list_projects()
            .await?
            .into_iter()
            .find(|p| same(&findery_core::support::path_map::normalize(&p.spec.root)));
        Ok(found.map(|p| p.id))
    }

    /// The pin a registration asks for: `None` keeps the current one, an empty
    /// alias removes it, any other must name a profile.
    fn requested_pin(&self, raw: Option<&str>) -> Result<Option<Option<ModelAlias>>> {
        match raw.map(str::trim) {
            None => Ok(None),
            Some("") => Ok(Some(None)),
            Some(alias) => Ok(Some(Some(self.pool.known(alias)?))),
        }
    }

    /// Every attached project, most recently updated first.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let homes: Vec<(ProjectId, String)> = self
            .homes
            .read()
            .iter()
            .map(|(id, home)| (id.clone(), home.clone()))
            .collect();
        let mut out = Vec::with_capacity(homes.len());
        for (id, home) in homes {
            let family = match self.family(&home) {
                Ok(family) => family,
                // Moved or detached since the snapshot: not a project to list.
                Err(_) if self.home_of(&id).ok().as_deref() != Some(home.as_str()) => continue,
                Err(_) => {
                    return Err(Error::Internal(format!(
                        "project `{id}` lives in bucket `{home}`, which is not open"
                    )));
                }
            };
            if let Some(row) = family.freshest_row(&id).await? {
                out.push(self.stamped(row));
            }
        }
        // The families each sorted their own rows; re-sorting here is what makes
        // one list out of several, and the id tiebreak keeps it deterministic.
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(out)
    }

    /// One attached project, read without waiting at its gate or building any
    /// embedder: a lookup must answer even while a sync holds the project and
    /// its model's runtime is down.
    pub async fn get_project(&self, id: &ProjectId) -> Result<Project> {
        let family = self.family(&self.home_of(id)?)?;
        match family.freshest_row(id).await? {
            Some(row) => Ok(self.stamped(row)),
            None => Err(Error::NotFound(format!("project `{id}`"))),
        }
    }

    /// A row with the model it uses right now: its pin, else the default. A
    /// pin this daemon does not define is shown as it is — the listing
    /// reports, the operation refuses.
    pub(crate) fn stamped(&self, mut project: Project) -> Project {
        project.model = Some(
            project
                .spec
                .model
                .clone()
                .unwrap_or_else(|| self.pool.default_alias()),
        );
        project
    }
}
