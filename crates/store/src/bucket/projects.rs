//! Project and file records: the state incremental sync compares against disk.

use super::Bucket;
use crate::schema::sql;
use findery_core::clock::now_unix;
use findery_core::ids::{ContentHash, ProjectId};
use findery_core::model::{FileRecord, Lang, Origin, Project, ProjectSpec};
use findery_core::{Error, Result};
use rusqlite::{Connection, Row, params};
use std::collections::HashMap;

impl Bucket {
    /// Creates or replaces a project definition.
    pub async fn put_project(&self, project: &Project) -> Result<()> {
        let id = project.id.to_string();
        let name = project.name.clone();
        let spec = serde_json::to_string(&project.spec)?;
        let host_root = project.host_root.clone();
        let created = project.created_at;
        let updated = now_unix();
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO projects (id, name, spec_json, host_root, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                     name = excluded.name,
                     spec_json = excluded.spec_json,
                     host_root = excluded.host_root,
                     updated_at = excluded.updated_at",
                params![id, name, spec, host_root, created, updated],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Fetches one project, or `None` when it was never registered.
    pub async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>> {
        let id = id.to_string();
        let bucket = self.name.clone();
        self.with_conn(move |c| {
            let mut stmt = c.prepare(PROJECT_SELECT).map_err(sql)?;
            let mut rows = stmt.query([&id]).map_err(sql)?;
            match rows.next().map_err(sql)? {
                Some(row) => Ok(Some(read_project(row, &bucket)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Same as [`Bucket::get_project`] but turns absence into an error, which
    /// is what every route that operates on a named project wants.
    pub async fn require_project(&self, id: &ProjectId) -> Result<Project> {
        self.get_project(id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("project `{id}`")))
    }

    /// Lists projects, most recently updated first.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let bucket = self.name.clone();
        self.with_conn(move |c| {
            let mut stmt = c
                .prepare(&format!("{PROJECT_SELECT_ALL} ORDER BY updated_at DESC"))
                .map_err(sql)?;
            let mut rows = stmt.query([]).map_err(sql)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql)? {
                out.push(read_project(row, &bucket)?);
            }
            Ok(out)
        })
        .await
    }

    /// Removes a project and everything attached to it, in one transaction.
    ///
    /// The vector backend is purged separately by the caller: this method owns
    /// the bucket, not the store.
    pub async fn delete_project(&self, id: &ProjectId) -> Result<bool> {
        let id = id.to_string();
        self.with_conn(move |c| {
            let tx = c.unchecked_transaction().map_err(sql)?;
            tx.execute("DELETE FROM chunks_fts WHERE project_id = ?1", [&id])
                .map_err(sql)?;
            tx.execute("DELETE FROM chunks WHERE project_id = ?1", [&id])
                .map_err(sql)?;
            tx.execute("DELETE FROM files WHERE project_id = ?1", [&id])
                .map_err(sql)?;
            tx.execute("DELETE FROM vectors WHERE project_id = ?1", [&id])
                .map_err(sql)?;
            let n = tx
                .execute("DELETE FROM projects WHERE id = ?1", [&id])
                .map_err(sql)?;
            tx.commit().map_err(sql)?;
            Ok(n > 0)
        })
        .await
    }

    /// Stamps the end of a successful sync.
    pub async fn touch_sync(&self, id: &ProjectId) -> Result<()> {
        let id = id.to_string();
        let now = now_unix();
        self.with_conn(move |c| {
            c.execute(
                "UPDATE projects SET last_sync_at = ?2 WHERE id = ?1",
                params![id, now],
            )
            .map_err(sql)?;
            Ok(())
        })
        .await
    }

    /// Every known file of a project, keyed by relative path.
    pub async fn file_map(&self, id: &ProjectId) -> Result<HashMap<String, FileRecord>> {
        let id = id.to_string();
        self.with_conn(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT path, lang, origin, content_hash, size, mtime_ns, chunk_count, indexed_at
                     FROM files WHERE project_id = ?1",
                )
                .map_err(sql)?;
            let mut rows = stmt.query([&id]).map_err(sql)?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next().map_err(sql)? {
                let rec = read_file(row)?;
                out.insert(rec.path.clone(), rec);
            }
            Ok(out)
        })
        .await
    }

    /// Inserts or updates one file record.
    pub async fn put_file(&self, project: &ProjectId, file: &FileRecord) -> Result<()> {
        let pid = project.to_string();
        let f = file.clone();
        self.with_conn(move |c| {
            put_file_sync(c, &pid, &f)?;
            Ok(())
        })
        .await
    }

    /// Drops a file record. Chunks are removed separately, by the caller that
    /// also owns removing their vectors.
    pub async fn delete_file_record(&self, project: &ProjectId, path: &str) -> Result<bool> {
        let (pid, path) = (project.to_string(), path.to_string());
        self.with_conn(move |c| {
            let n = c
                .execute(
                    "DELETE FROM files WHERE project_id = ?1 AND path = ?2",
                    params![pid, path],
                )
                .map_err(sql)?;
            Ok(n > 0)
        })
        .await
    }

    /// Project and chunk counts for the whole bucket.
    ///
    /// INTENT: what `GET /v1/buckets` reports, so it must count rows, not
    /// files: a bucket with three projects and no chunks is a created-but-
    /// unindexed one, and the two numbers say so.
    pub async fn totals(&self) -> Result<(u64, u64)> {
        self.with_conn(move |c| {
            let row = c
                .query_row(
                    "SELECT (SELECT count(*) FROM projects), (SELECT count(*) FROM chunks)",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .map_err(sql)?;
            Ok((row.0.max(0) as u64, row.1.max(0) as u64))
        })
        .await
    }

    /// File and chunk counts plus the last sync timestamp.
    pub async fn counts(&self, project: &ProjectId) -> Result<(u64, u64, Option<i64>)> {
        let pid = project.to_string();
        self.with_conn(move |c| {
            let files: i64 = c
                .query_row(
                    "SELECT count(*) FROM files WHERE project_id = ?1",
                    [&pid],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            let chunks: i64 = c
                .query_row(
                    "SELECT count(*) FROM chunks WHERE project_id = ?1",
                    [&pid],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            let last: Option<i64> = c
                .query_row(
                    "SELECT last_sync_at FROM projects WHERE id = ?1",
                    [&pid],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            Ok((files as u64, chunks as u64, last))
        })
        .await
    }
}

/// Shared by [`Bucket::put_file`] and the chunk-writing transaction.
pub(super) fn put_file_sync(c: &Connection, project: &str, f: &FileRecord) -> Result<()> {
    c.execute(
        "INSERT INTO files
             (project_id, path, lang, origin, content_hash, size, mtime_ns, chunk_count, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(project_id, path) DO UPDATE SET
             lang = excluded.lang,
             origin = excluded.origin,
             content_hash = excluded.content_hash,
             size = excluded.size,
             mtime_ns = excluded.mtime_ns,
             chunk_count = excluded.chunk_count,
             indexed_at = excluded.indexed_at",
        params![
            project,
            f.path,
            f.lang.as_str(),
            f.origin.as_str(),
            f.content_hash.as_str(),
            f.size as i64,
            f.mtime_ns,
            f.chunk_count as i64,
            f.indexed_at,
        ],
    )
    .map_err(sql)?;
    Ok(())
}

const PROJECT_SELECT: &str =
    "SELECT id, name, spec_json, host_root, created_at, updated_at FROM projects WHERE id = ?1";
const PROJECT_SELECT_ALL: &str =
    "SELECT id, name, spec_json, host_root, created_at, updated_at FROM projects";

/// `bucket` is the name of the bucket the row was read out of, not a column:
/// a project's bucket is where it lives, and a column could disagree with it.
fn read_project(row: &Row<'_>, bucket: &str) -> Result<Project> {
    let id: String = row.get(0).map_err(sql)?;
    let spec_json: String = row.get(2).map_err(sql)?;
    let spec: ProjectSpec = serde_json::from_str(&spec_json)?;
    Ok(Project {
        id: ProjectId::new(id)?,
        name: row.get(1).map_err(sql)?,
        bucket: bucket.to_string(),
        spec,
        host_root: row.get(3).map_err(sql)?,
        created_at: row.get(4).map_err(sql)?,
        updated_at: row.get(5).map_err(sql)?,
    })
}

fn read_file(row: &Row<'_>) -> Result<FileRecord> {
    let lang: String = row.get(1).map_err(sql)?;
    let origin: String = row.get(2).map_err(sql)?;
    let hash: String = row.get(3).map_err(sql)?;
    let size: i64 = row.get(4).map_err(sql)?;
    let chunk_count: i64 = row.get(6).map_err(sql)?;
    Ok(FileRecord {
        path: row.get(0).map_err(sql)?,
        lang: Lang::parse(&lang).unwrap_or(Lang::Other),
        origin: Origin::parse(&origin).unwrap_or(Origin::File),
        content_hash: ContentHash::from_hex(hash),
        size: size.max(0) as u64,
        mtime_ns: row.get(5).map_err(sql)?,
        chunk_count: chunk_count.max(0) as u32,
        indexed_at: row.get(7).map_err(sql)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str) -> Project {
        Project {
            id: ProjectId::new(id).expect("id"),
            name: id.to_string(),
            bucket: findery_core::model::DEFAULT_BUCKET.into(),
            spec: ProjectSpec {
                root: "/workspaces/demo".into(),
                ..ProjectSpec::default()
            },
            host_root: "C:/demo".into(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn file(path: &str, hash: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            lang: Lang::Rust,
            origin: Origin::File,
            content_hash: ContentHash::from_hex(hash),
            size: 10,
            mtime_ns: 42,
            chunk_count: 2,
            indexed_at: 7,
        }
    }

    #[tokio::test]
    async fn projects_round_trip_and_upsert() {
        let cat = Bucket::open_in_memory().expect("bucket");
        cat.put_project(&project("demo")).await.expect("insert");
        let mut p = project("demo");
        p.name = "renamed".into();
        cat.put_project(&p).await.expect("update");

        let got = cat
            .require_project(&ProjectId::new("demo").expect("id"))
            .await
            .expect("get");
        assert_eq!(got.name, "renamed");
        assert_eq!(got.spec.root, "/workspaces/demo");
        assert_eq!(cat.list_projects().await.expect("list").len(), 1);
    }

    /// The bucket a project belongs to is read off the file it came from, so a
    /// row cannot claim one bucket while living in another.
    #[tokio::test]
    async fn a_project_reports_the_bucket_it_was_read_out_of() {
        let cat = Bucket::open_in_memory().expect("bucket");
        cat.put_project(&project("demo")).await.expect("insert");
        let got = cat
            .require_project(&ProjectId::new("demo").expect("id"))
            .await
            .expect("get");
        assert_eq!(got.bucket, cat.name());
    }

    #[tokio::test]
    async fn a_missing_project_is_not_found_not_a_crash() {
        let cat = Bucket::open_in_memory().expect("bucket");
        let err = cat
            .require_project(&ProjectId::new("ghost").expect("id"))
            .await
            .expect_err("must fail");
        assert_eq!(err.code(), "not_found");
    }

    #[tokio::test]
    async fn files_are_keyed_by_path_and_overwritten_in_place() {
        let cat = Bucket::open_in_memory().expect("bucket");
        let id = ProjectId::new("demo").expect("id");
        cat.put_project(&project("demo")).await.expect("insert");
        cat.put_file(&id, &file("src/a.rs", "h1"))
            .await
            .expect("put");
        cat.put_file(&id, &file("src/a.rs", "h2"))
            .await
            .expect("put again");

        let map = cat.file_map(&id).await.expect("map");
        assert_eq!(map.len(), 1);
        assert_eq!(map["src/a.rs"].content_hash.as_str(), "h2");

        assert!(
            cat.delete_file_record(&id, "src/a.rs")
                .await
                .expect("delete")
        );
        assert!(cat.file_map(&id).await.expect("map").is_empty());
    }

    #[tokio::test]
    async fn deleting_a_project_takes_its_files_with_it() {
        let cat = Bucket::open_in_memory().expect("bucket");
        let id = ProjectId::new("demo").expect("id");
        cat.put_project(&project("demo")).await.expect("insert");
        cat.put_file(&id, &file("src/a.rs", "h1"))
            .await
            .expect("put");
        assert!(cat.delete_project(&id).await.expect("delete"));
        assert!(cat.file_map(&id).await.expect("map").is_empty());
        assert!(!cat.delete_project(&id).await.expect("delete again"));
    }
}
