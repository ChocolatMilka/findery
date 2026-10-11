//! Vector export: take the index somewhere else.
//!
//! INTENT: an index you cannot get out of is a lock-in. Export reads the
//! bucket — chunks plus the content-addressed cache — so it works identically
//! whichever vector backend is live, and it writes plain formats any tool can
//! read: JSONL for a one-shot load, or a raw `float32` matrix plus sidecar
//! metadata for `numpy.fromfile` and friends.

use crate::bucket::Bucket;
use findery_core::ids::ProjectId;
use findery_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// On-disk shape of an export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// One JSON object per line, vector inlined. Portable, ~14 bytes/float.
    #[default]
    Jsonl,
    /// A single JSON array. Convenient, but loads whole into memory.
    Json,
    /// `vectors.f32` (row-major little-endian) + `meta.jsonl` + `manifest.json`.
    /// The compact option: 4 bytes per float, loadable with one `fromfile`.
    Binary,
}

impl ExportFormat {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "jsonl" | "ndjson" => Ok(Self::Jsonl),
            "json" => Ok(Self::Json),
            "binary" | "f32" | "npy" | "raw" => Ok(Self::Binary),
            other => Err(Error::Invalid(format!(
                "unknown export format `{other}`; expected jsonl, json or binary"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            Self::Json => "json",
            Self::Binary => "binary",
        }
    }
}

// The report is wire vocabulary: defined in L0 so the Rust client can decode it
// without depending on this crate.
pub use findery_core::api::ExportReport;

/// Sidecar written next to a binary export.
#[derive(Serialize)]
struct Manifest<'a> {
    project: &'a str,
    model: &'a str,
    recipe: &'a str,
    dim: usize,
    count: u64,
    dtype: &'static str,
    layout: &'static str,
    normalized: bool,
    metric: &'static str,
}

/// Exports one project's vectors and metadata into `dest_dir/<project>/`.
pub async fn export_project(
    bucket: &Bucket,
    project: &ProjectId,
    recipe: &str,
    model: &str,
    format: ExportFormat,
    dest_dir: &Path,
) -> Result<ExportReport> {
    let chunks = bucket.all_chunks(project).await?;
    let hashes: Vec<_> = chunks.iter().map(|c| c.content_hash.clone()).collect();
    let vectors = bucket.cached_vectors(recipe, hashes).await?;

    let dir: PathBuf = dest_dir.join(project.as_str());
    tokio::fs::create_dir_all(&dir).await?;

    let dim = vectors.values().next().map(Vec::len).unwrap_or(0);
    let mut written = Vec::new();
    let mut matched = 0u64;
    let mut bytes = 0u64;

    match format {
        ExportFormat::Jsonl | ExportFormat::Json => {
            let name = if format == ExportFormat::Jsonl {
                "vectors.jsonl"
            } else {
                "vectors.json"
            };
            let path = dir.join(name);
            let mut out = tokio::fs::File::create(&path).await?;
            if format == ExportFormat::Json {
                out.write_all(b"[\n").await?;
            }
            let mut first = true;
            for chunk in &chunks {
                let Some(vector) = vectors.get(chunk.content_hash.as_str()) else {
                    continue;
                };
                matched += 1;
                let line = serde_json::to_string(&Record::new(chunk, vector))?;
                if format == ExportFormat::Json && !first {
                    out.write_all(b",\n").await?;
                }
                out.write_all(line.as_bytes()).await?;
                if format == ExportFormat::Jsonl {
                    out.write_all(b"\n").await?;
                }
                bytes += line.len() as u64 + 1;
                first = false;
            }
            if format == ExportFormat::Json {
                out.write_all(b"\n]\n").await?;
            }
            out.flush().await?;
            written.push(name.to_string());
        }
        ExportFormat::Binary => {
            let vec_path = dir.join("vectors.f32");
            let meta_path = dir.join("meta.jsonl");
            let mut vec_out = tokio::fs::File::create(&vec_path).await?;
            let mut meta_out = tokio::fs::File::create(&meta_path).await?;
            for chunk in &chunks {
                let Some(vector) = vectors.get(chunk.content_hash.as_str()) else {
                    continue;
                };
                if vector.len() != dim {
                    return Err(Error::Store(format!(
                        "chunk {} has {} dimensions, expected {dim}",
                        chunk.id,
                        vector.len()
                    )));
                }
                matched += 1;
                let mut raw = Vec::with_capacity(dim * 4);
                for x in vector {
                    raw.extend_from_slice(&x.to_le_bytes());
                }
                vec_out.write_all(&raw).await?;
                bytes += raw.len() as u64;

                let line = serde_json::to_string(&Record::new(chunk, &[]))?;
                meta_out.write_all(line.as_bytes()).await?;
                meta_out.write_all(b"\n").await?;
            }
            vec_out.flush().await?;
            meta_out.flush().await?;

            let manifest = Manifest {
                project: project.as_str(),
                model,
                recipe,
                dim,
                count: matched,
                dtype: "float32",
                layout: "row-major",
                normalized: true,
                metric: "cosine",
            };
            let manifest_path = dir.join("manifest.json");
            tokio::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?).await?;
            written.extend([
                "vectors.f32".into(),
                "meta.jsonl".into(),
                "manifest.json".into(),
            ]);
        }
    }

    Ok(ExportReport {
        project: project.to_string(),
        format: format.as_str().to_string(),
        directory: dir.display().to_string(),
        files: written,
        chunks: chunks.len() as u64,
        vectors: matched,
        dim,
        bytes,
    })
}

/// One exported row. `vector` is skipped when empty so the binary sidecar stays
/// metadata-only.
#[derive(Serialize)]
struct Record<'a> {
    id: &'a str,
    path: &'a str,
    lang: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    breadcrumb: &'a Option<String>,
    start_line: u32,
    end_line: u32,
    content_hash: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "<[f32]>::is_empty")]
    vector: &'a [f32],
}

impl<'a> Record<'a> {
    fn new(chunk: &'a findery_core::model::Chunk, vector: &'a [f32]) -> Self {
        Self {
            id: chunk.id.as_str(),
            path: &chunk.path,
            lang: chunk.lang.as_str(),
            kind: chunk.kind.as_str(),
            symbol: &chunk.symbol,
            breadcrumb: &chunk.breadcrumb,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            content_hash: chunk.content_hash.as_str(),
            text: &chunk.text,
            vector,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use findery_core::ids::{ChunkId, ContentHash};
    use findery_core::model::{Chunk, ChunkKind, Lang, Project, ProjectSpec};

    async fn seeded() -> (Bucket, ProjectId, tempfile::TempDir) {
        let cat = Bucket::open_in_memory().expect("bucket");
        let p = ProjectId::new("demo").expect("id");
        cat.put_project(&Project {
            id: p.clone(),
            name: "demo".into(),
            bucket: cat.name().to_string(),
            spec: ProjectSpec::default(),
            host_root: "C:/demo".into(),
            created_at: 1,
            updated_at: 1,
        })
        .await
        .expect("project");

        let text = "fn main() {}";
        let hash = ContentHash::of(text.as_bytes());
        let chunk = Chunk {
            id: ChunkId::derive(&p, "src/main.rs", 0),
            project: p.clone(),
            path: "src/main.rs".into(),
            lang: Lang::Rust,
            ordinal: 0,
            kind: ChunkKind::Function,
            symbol: Some("main".into()),
            breadcrumb: None,
            start_line: 1,
            end_line: 1,
            content_hash: hash.clone(),
            text: text.into(),
        };
        cat.replace_file_chunks(&p, "src/main.rs", vec![chunk])
            .await
            .expect("chunks");
        cat.put_cached_vectors("r:4", vec![(hash, vec![0.5, 0.5, 0.5, 0.5])])
            .await
            .expect("cache");
        (cat, p, tempfile::tempdir().expect("tempdir"))
    }

    #[tokio::test]
    async fn jsonl_export_carries_metadata_and_vectors() {
        let (cat, p, dir) = seeded().await;
        let report = export_project(&cat, &p, "r:4", "m", ExportFormat::Jsonl, dir.path())
            .await
            .expect("export");
        assert_eq!(report.vectors, 1);
        assert_eq!(report.dim, 4);

        let body =
            std::fs::read_to_string(dir.path().join("demo/vectors.jsonl")).expect("read back");
        let row: serde_json::Value = serde_json::from_str(body.trim()).expect("parse");
        assert_eq!(row["path"], "src/main.rs");
        assert_eq!(row["vector"].as_array().expect("vector").len(), 4);
    }

    #[tokio::test]
    async fn binary_export_writes_a_loadable_matrix_and_manifest() {
        let (cat, p, dir) = seeded().await;
        let report = export_project(&cat, &p, "r:4", "qwen3", ExportFormat::Binary, dir.path())
            .await
            .expect("export");
        assert_eq!(report.files.len(), 3);

        let raw = std::fs::read(dir.path().join("demo/vectors.f32")).expect("matrix");
        assert_eq!(raw.len(), 4 * 4, "one row of four float32");

        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("demo/manifest.json")).expect("manifest"),
        )
        .expect("parse");
        assert_eq!(manifest["dim"], 4);
        assert_eq!(manifest["dtype"], "float32");
        assert_eq!(manifest["model"], "qwen3");

        // Metadata sidecar must not duplicate the vectors.
        let meta = std::fs::read_to_string(dir.path().join("demo/meta.jsonl")).expect("meta");
        assert!(!meta.contains("\"vector\""));
    }

    #[tokio::test]
    async fn a_chunk_with_no_cached_vector_is_reported_not_invented() {
        let (cat, p, dir) = seeded().await;
        // A recipe nothing was embedded under.
        let report = export_project(&cat, &p, "other:4", "m", ExportFormat::Jsonl, dir.path())
            .await
            .expect("export");
        assert_eq!(report.chunks, 1);
        assert_eq!(report.vectors, 0);
    }

    #[test]
    fn format_parsing_refuses_the_unknown() {
        assert_eq!(
            ExportFormat::parse("JSONL").expect("parse"),
            ExportFormat::Jsonl
        );
        assert_eq!(
            ExportFormat::parse("npy").expect("parse"),
            ExportFormat::Binary
        );
        assert!(ExportFormat::parse("parquet").is_err());
    }
}
