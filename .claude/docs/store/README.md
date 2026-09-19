# `findery-store` — catalog, vector stores, export

Two things live here, deliberately separated.

**The catalog** (`catalog/`): a SQLite file that carries the projects, the
files, the chunks, the content-addressed vector cache, and the full-text
index. Always present, whatever the vector backend. **This is the file to
back up.**

**The vector store** (`sqlite_store.rs`, `qdrant.rs`): where vectors are
written and searched. Reconstructible from the catalog — that's what makes
swapping it possible: a config change plus a re-sync.

---

## 0. Where it lives

One file, `$FINDERY_DATA_DIR/catalog.db`, holding every project.

| | Native daemon *(default)* | All-Docker |
|---|---|---|
| Catalog + vectors | `%LOCALAPPDATA%/findery` or `~/.local/share/findery` | volume `findery_findery-data` |
| Exports | `$FINDERY_DATA_DIR/exports` | `/data/exports` |
| Model weights | volume `findery_hf-cache` | same |

`finderyd --check` prints the resolved paths. `export_dir` follows
`data_dir` unless it was pinned explicitly.

Measured footprint: **~10.7 kB per chunk at 1024 dims**, so ~535 MB for a
500k-line codebase. Note that with the `sqlite` backend each vector is stored
**twice** — once in `embeddings` (the content-addressed cache) and once in
`vectors` (the project's live set, which also carries the payload). Roadmap R2b
carries the measurement and the deduplication path.

## 1. The catalog

### Schema (`schema.rs`, `user_version = 1`)

| Table | Role |
|-------|------|
| `projects` | definition + `last_sync_at` |
| `files` | `(size, mtime_ns, content_hash)` — sync gates 1 and 2 |
| `chunks` | the text, the lines, the symbol, the `content_hash` |
| `embeddings` | **the cache**, keyed `(recipe, content_hash)` |
| `vectors` | live vectors for the `sqlite` backend |
| `chunks_fts` | FTS5, the BM25 leg of hybrid search |
| `meta` | schema version, `embedding_recipe` |

`chunks` and `chunks_fts` are written **in the same transaction**
(CONV-027): a full-text index that lags returns hits pointing at text that's
gone, which is worse than having no BM25 leg at all.

### Concurrency

WAL + `synchronous = NORMAL`. A single `Mutex<Connection>`, every method
hops onto `spawn_blocking`. It's the right trade-off for a single-writer
daemon: the queries are indexed lookups, and **vector search never touches
this connection**.

### The vector cache — the heart of the economics

Keyed `(recipe, content_hash)`, never the location. Consequences:

- moving a function between files: free
- renaming a module: free
- re-cloning the repo: free
- `sync --full` on an unchanged repo: **zero embeddings**

`gc_embeddings()` exists but is **never** automatic: the cache is precisely
what makes a moved file free, collecting it eagerly would defeat its own
purpose.

---

## 2. The `sqlite` backend — exact in-memory scan

Vectors are durable in the `vectors` table; search runs on a **contiguous
`f32` matrix in RAM**, built lazily, L2-normalized. Cosine = dot product.
The kernel uses eight independent accumulators to break the dependency
chain and let LLVM emit vectorized FMAs.

This is **not** "search with SQL". SQLite only handles persistence.

### The numbers — `make bench` (release, re-run on your own machine)

| Chunks | Dim | RAM | Naive | Shipped kernel |
|--------|-----|-----|------|-------------|
| 10,000 | 1024 | 41 MB | 3.6 ms | **2.4 ms** |
| 50,000 | 1024 | 205 MB | 18.2 ms | **12.1 ms** |
| 200,000 | 1024 | 819 MB | 73.9 ms | **50.1 ms** |
| 10,000 | 2560 | 102 MB | 9.1 ms | **5.7 ms** |
| 50,000 | 2560 | 512 MB | 44.6 ms | **28.1 ms** |
| 200,000 | 2560 | 2,048 MB | 182.2 ms | **113.7 ms** |

Order of magnitude: a ~500 k-line Rust workspace produces ~40–60 k chunks.
So we're **under 15 ms per query**, below the latency floor of the agent
asking the question, with **exact** recall and zero extra service.

### Why not an embedded ANN index

Also measured, with `hnsw_rs` (20,000 vectors, 1024 dims, release):

- search: **1.14 ms/query** — 4x faster than the scan, effectively
- build: **76 seconds** for 20,000 vectors

That's the wrong trade-off **here**. A code index mutates constantly: every
file save re-inserts. Paying several milliseconds per insert to save a few
milliseconds per query, on a corpus that changes more often than it's
queried, moves the cost to the wrong place. When the scan stops being
enough, the right answer is an engine that handles incremental insertion
**and** ANN — that is, Qdrant.

---

## 3. The `qdrant` backend — the local vector database

A real vector engine, **local**, in the same `docker compose`:

```bash
FINDERY_STORE_BACKEND=qdrant docker compose --profile qdrant up -d
```

One collection per project (`se_<project_id>`), `Cosine` distance, HNSW,
optional int8 quantization (`scalar_quantization = true`, 4x less RAM for
~1% recall).

**REST API, not the gRPC client**: the dependency is `reqwest`, already
present for the embedder, and our release cadence no longer keeps up with
that of a client library chasing the server's majors.

**Filters**: `lang` and `kind` are pushed server-side (with a `keyword`
index). Path globs and symbol substrings have no Qdrant equivalent, so we
over-fetch (×8, capped at 1024) and filter locally with `CompiledFilter` —
the **same** semantics as the sqlite backend (CONV-011). An approximate
translation would be worse than a second pass over a few hundred lines.

### Switching to it

```bash
docker compose --profile qdrant up -d qdrant   # the only container you need
FINDERY_STORE_BACKEND=qdrant FINDERY_QDRANT_URL=http://127.0.0.1:6333 finderyd
findery sync
```

The catalog does not move, so **the switch re-embeds nothing**: every vector is
already in the content-addressed cache and gets upserted into the new store.

The daemon notices on its own that the store is empty while the catalog is not,
upgrades the sync to a full pass and says so in the report. Without that, an
incremental pass would find every file unchanged, write nothing, and leave
search silently returning nothing.

### When to switch

| Signal | Backend |
|--------|---------|
| < ~150 k chunks, one machine | `sqlite` |
| Measured latency > 50 ms | `qdrant` |
| Daemon RAM is a problem | `qdrant` (+ quantization) |
| Multiple machines on a shared index | `qdrant` |
| Exact recall required | `sqlite` |

Switching = changing `FINDERY_STORE_BACKEND` and re-running a sync. The catalog
doesn't move, and since vectors are cached, **the sync re-embeds nothing**.

---

## 4. Export — never locked in

`export_project()` reads the **catalog** (chunks + cache), so it works
identically regardless of which backend is live.

| Format | Files | For |
|--------|----------|-----|
| `jsonl` | `vectors.jsonl` | one-shot loading, human-readable |
| `json` | `vectors.json` | a single array, convenient and heavy |
| `binary` | `vectors.f32` + `meta.jsonl` + `manifest.json` | 4 bytes/float, `numpy.fromfile` |

```python
import json, numpy as np
m = json.load(open("manifest.json"))
V = np.fromfile("vectors.f32", dtype=np.float32).reshape(-1, m["dim"])
meta = [json.loads(l) for l in open("meta.jsonl", encoding="utf-8")]
assert len(meta) == V.shape[0]
```

A chunk with no vector under the current recipe is **counted** in the
report (`chunks` vs `vectors`), never invented or silently dropped.
