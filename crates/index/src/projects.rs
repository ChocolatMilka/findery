//! What a project is, and where it lives.
//!
//! Split from `lib.rs`: registering a project decides everything about it at
//! once — how it is fed, which bucket holds its rows, what the walker must skip —
//! and that reads better in one piece than interleaved with search and export.

use crate::Indexer;
use findery_core::api::RegisterProject;
use findery_core::clock::now_unix;
use findery_core::ids::ProjectId;
use findery_core::model::{Project, ProjectSpec};
use findery_core::{Error, Result};
use std::path::Path;

impl Indexer {
    /// Registers a project, or updates it when the id already exists.
    ///
    /// The bucket is chosen here and only here. Naming one the project does not
    /// currently live in *moves* it — a project is in exactly one bucket, and
    /// that is what makes `findery init --bucket work` a move rather than a
    /// silent duplicate.
    pub async fn register(&self, req: RegisterProject) -> Result<Project> {
        if req.root.trim().is_empty() {
            return Err(Error::Invalid("root must not be empty".into()));
        }
        let container_root = self.mapper.to_container(&req.root);
        // Whether the daemon can read the directory decides HOW the project gets
        // fed, not whether it can be indexed. Unreadable means the client walks
        // it and pushes the contents — a deployment fact, not an error.
        let push = req
            .push
            .unwrap_or_else(|| !Path::new(&container_root).is_dir());

        let defaults = &self.cfg.project_defaults;
        let name = req
            .name
            .clone()
            .or_else(|| basename(&container_root))
            .unwrap_or_else(|| "project".into());
        let id = match &req.id {
            Some(raw) => ProjectId::new(raw.clone())?,
            None => ProjectId::slugify(&name)?,
        };

        // Which bucket, and where its file goes. The daemon resolves rather than
        // the client because it owns the data directory and the mounts, and
        // because it is the only side that knows whether it can write inside
        // this project at all.
        let resolved = req.bucket.clone().unwrap_or_default().resolve(
            &self.cfg.store,
            &id,
            &container_root,
            !push,
        )?;
        let target = resolved.name.clone();
        if resolved.substituted {
            // The requested bucket is on a disk this daemon cannot write, so it
            // gets a different one. The answer carries what was used, and a
            // substitution worth making is worth saying out loud.
            tracing::warn!(
                project = %id,
                using = %resolved.path,
                "the requested bucket is not reachable; using one beside the data directory"
            );
        }

        let mut spec = ProjectSpec {
            root: container_root,
            include: req.include.unwrap_or_default(),
            exclude: req.exclude.unwrap_or_else(|| defaults.exclude.clone()),
            extensions: req
                .extensions
                .unwrap_or_else(|| defaults.extensions.clone()),
            follow_gitignore: req.follow_gitignore.unwrap_or(defaults.follow_gitignore),
            include_hidden: req.include_hidden.unwrap_or(defaults.include_hidden),
            max_file_bytes: req.max_file_bytes.unwrap_or(defaults.max_file_bytes),
            push,
            // The project's own defaults, carried to the daemon so a transport
            // that never read the manifest can still resolve them.
            scopes: req.scopes.unwrap_or_default(),
            instruction: req.instruction,
            task: req.task,
        };
        // A bucket can live inside the project it indexes, and indexing the index
        // is a write loop: every sync would see a file it had just written.
        for pattern in self.exclusions_for(&resolved.path, &spec.root) {
            if !spec.exclude.contains(&pattern) {
                spec.exclude.push(pattern);
            }
        }

        let handle = self.ensure_bucket_at(&target, Some(&resolved.path)).await?;
        self.move_project(&id, &target).await?;

        let existing = handle.bucket.get_project(&id).await?;
        let project = Project {
            // A pushed project has no daemon-side path to map back from.
            host_root: if push {
                req.root.clone()
            } else {
                self.mapper.to_host(&spec.root)
            },
            id,
            name,
            bucket: target.clone(),
            spec,
            created_at: existing.map(|p| p.created_at).unwrap_or_else(now_unix),
            updated_at: now_unix(),
        };
        handle.bucket.put_project(&project).await?;
        handle
            .store
            .ensure_project(&project.id, self.embedder.dim())
            .await?;
        self.remember_home(&project.id, &target);
        Ok(project)
    }

    /// The globs that keep a bucket out of the tree it indexes.
    ///
    /// Two locks, both needed: the fixed pattern covers the ordinary case — the
    /// walker prunes `.findery` structurally anyway — and the resolved path
    /// covers a bucket pointed at an arbitrary location *inside* the root, which
    /// no fixed pattern can name.
    fn exclusions_for(&self, bucket_path: &str, root: &str) -> Vec<String> {
        let mut patterns = vec![".findery/**".to_string(), "**/.findery/**".to_string()];
        let root = root.trim_end_matches(['/', '\\']);
        let normalized = bucket_path.replace('\\', "/");
        let prefix = format!("{}/", root.replace('\\', "/"));
        if let Some(rel) = normalized.strip_prefix(&prefix)
            && !rel.is_empty()
        {
            // The journal files are named after the database, so a pattern that
            // named only the database would index its write-ahead log.
            for suffix in ["", "-wal", "-shm"] {
                patterns.push(format!("{rel}{suffix}"));
            }
        }
        patterns
    }

    /// Takes a project out of the bucket it used to live in, when it is moving.
    ///
    /// Its vectors go with it: they were built for that bucket's store, and
    /// leaving them behind would keep paying for an index nothing points at.
    async fn move_project(&self, id: &ProjectId, target: &str) -> Result<()> {
        let Ok(current) = self.home_of(id) else {
            return Ok(());
        };
        if current == target {
            return Ok(());
        }
        let previous = self.handle(&current)?;
        previous.store.drop_project(id).await?;
        previous.bucket.delete_project(id).await?;
        self.forget_home(id);
        Ok(())
    }

    /// Every registered project, most recently updated first.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let mut out = Vec::new();
        for handle in self.handles() {
            out.extend(handle.bucket.list_projects().await?);
        }
        // The buckets each sorted their own rows; re-sorting here is what makes
        // one list out of several, and the id tiebreak keeps it deterministic.
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(out)
    }

    pub async fn get_project(&self, id: &ProjectId) -> Result<Project> {
        self.bucket_for(id)?.bucket.require_project(id).await
    }

    /// Removes a project, its chunks and its vectors.
    pub async fn forget_project(&self, id: &ProjectId) -> Result<bool> {
        let handle = self.bucket_for(id)?;
        // The vector store goes first: a bucket row with no vectors is
        // recoverable, an orphaned collection is not.
        handle.store.drop_project(id).await?;
        let removed = handle.bucket.delete_project(id).await?;
        self.forget_home(id);
        Ok(removed)
    }
}

/// Last path segment of a `/`-separated path.
fn basename(path: &str) -> Option<String> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_survives_trailing_slashes() {
        assert_eq!(basename("/workspaces/axle/"), Some("axle".into()));
        assert_eq!(basename("/"), None);
    }
}
