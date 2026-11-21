# `findery-store` — bucket, vector stores, export

Two things live here, deliberately separated.

- **The bucket** (`bucket/`) — a SQLite file that carries the projects, the
  files, the chunks, the content-addressed vector cache, and the full-text
  index. Always present, whatever the vector backend. **This is the file to
  back up.**
- **The vector store** (`vectors/sqlite.rs`, `vectors/qdrant.rs`) — where vectors are
  written and searched. Reconstructible from the bucket — that's what makes
  swapping it possible: a config change plus a re-sync.

---

## 0. Where it lives

One SQLite file per bucket **and per model** (a *member*, below), and a daemon
serves as many buckets as you create. A bucket is named by its **base path**:

| Bucket                                             | Base path                                    |
|----------------------------------------------------|----------------------------------------------|
| the default one                                    | `$FINDERY_DATA_DIR/bucket.db`                |
| any other                                          | `$FINDERY_DATA_DIR/buckets/<name>/bucket.db` |
| a project's own *(the default for `findery init`)* | `<project>/.findery/bucket.db`               |

`bucket_path` / `buckets_dir` (`config/paths.rs`) are the only places that
spell those layouts, so a caller never assembles one by hand.

The files on disk are the base's members, `bucket.<slug>-<tag>.db`; the base
path itself is only a name, and holds a file only while a legacy one has not
been adopted yet.

|                  | Native daemon *(default)*                            | All-Docker                    |
|------------------|------------------------------------------------------|-------------------------------|
| Bucket + vectors | `%LOCALAPPDATA%/findery` or `~/.local/share/findery` | volume `findery_findery-data` |
| Exports          | `$FINDERY_DATA_DIR/exports`                          | `/data/exports`               |
| Model weights    | volume `findery_hf-cache`                            | same                          |

`finderyd --check` prints the resolved paths. `export_dir` follows
`data_dir` unless it was pinned explicitly.

A project's bucket is **not a column** — it is which family's files hold its
row. The name is stamped onto `Project.bucket` when the row is read, and the
model it uses now onto `Project.model` (its pin, else the default), so there is
no second place a project's index or model could be recorded and no way for the
two to disagree.

That is also what makes `findery bucket rm` a deletion of the family's files
rather than a cascade.

> **Note** — with the `sqlite` backend each vector is stored **twice**: once in
> `embeddings` (the content-addressed cache) and once in `vectors` (the
> project's live set, which also carries the payload). Roadmap R2b carries the
> measurement and the deduplication path.

### Families and members (`files/layout.rs`)

A bucket on disk is a **family**: a name plus a base path `dir/stem.db` — the
path `bucket_path`, a project manifest (`path = ".findery/bucket.db"`) and
`buckets.toml` already hold, unchanged. A family's files are its **members**,
one per vector space:

```text
<dir>/<stem>.db                      the legacy member: the one file a bucket had
                                     before members; its recipe is read, not named
<dir>/<stem>.<slug>-<tag>.db         one recipe's member, e.g.
                                     bucket.harrier-oss-v1-0-6b-zvpmtsdf.db
```

The slug is cosmetic (never parsed); the tag is the recipe's 8-character
base32 fingerprint (`findery_core::embedding::recipe`).

Only `config::bucket_member_path` spells a member's name and only
`config::parse_member_name` reads one back, so a directory listing is the whole
truth about a family: `scan(&FamilyPath)` lists the legacy file and every
tagged member, and ignores `-wal`/`-shm` and another family's files
(`bucket.foo.db`, `bucket.foo.x-<tag>.db`).

`FamilyPath::from_base` accepts a member path too and normalizes it back to its
base.

`list_families` (what the daemon lists buckets with) counts a directory under
`buckets/` as a bucket as soon as it holds **any** member, legacy or tagged.

`family_absent` is what a registry entry is pruned on, and it needs **proof**:
the directory lists and holds none of the family's files. A directory that is
not there at all (an unmounted drive, an offline share) or cannot be listed
proves nothing — forgetting where an index lives is not something a later boot
can undo.

`remove_file_set` deletes a member with its journals, as a step of a removal:
a file another process holds is `Error::busy_removal`, which says the removal
stopped part-way — never "nothing was changed". `delete_bucket` removes a
derived-path bucket's directory the same way.

The engine routes every operation through a member — `findery-index`'s
`storage/families.rs`, described in `.claude/docs/index/README.md` §0: a read
finds the member of the project's model, a write creates it. Nothing creates a
base `bucket.db`.

### Adoption of a legacy file (`files/adopt.rs`)

`adopt_legacy(&FamilyPath)` renames a family's base file to the member of the
recipe it records. Each step is a reason to stop with the file untouched:

1. Open it read-write **without creating** (busy timeout
   `store.peek_busy_timeout_ms`, 2 s) and read `meta.embedding_recipe` — none:
   `Unclaimed`, left as it is.
2. The target member already exists: `Stale`, nothing touched (never
   overwrite).
3. `wal_checkpoint(TRUNCATE)` with its row **checked** (`busy == 0`,
   `log == checkpointed`), then close. A `-wal`/`-shm` still on disk afterwards
   means another connection has the file open (the last one to close deletes
   both).
4. Rename the database, then any journal that appeared in between: `Adopted`.

Any failure that leaves the file in place — a lock (Windows os error 32/5), a
busy reader, a read-only volume, a recipe no member can be named after
(`model:0`) — is `KeptInPlace { reason }`, **never an `Err`**: the file in place
is a working legacy member.

`Err` means exactly one thing, the database moved and a journal behind it could
not follow, and the message names the manual `mv`. Idempotent: a second run
answers `Absent`.

`adopt_all(cfg, &registry)` visits the default bucket, every `buckets/<name>/`
and every registry location once each, and returns one `AdoptNotice` per bucket
that had a base file (`needs_attention()` for `Stale`, `KeptInPlace` and
errors). It is blocking and never fails.

It **skips** a bucket (other than `default`) whose files hold projects that are
**all** detached — the same rule the boot uses to leave such a family closed.
`findery detach` told the user those files stay put and unlocked; renaming them
would move what they may already have moved or backed up.

The project ids are peeked with `read_project_ids_no_create`; a file that
cannot be read is not skipped, so adoption reports it. The first start after
one of its projects is attached again adopts it.

The vocabulary migration (`catalog.db` → `bucket.db`, `files/migrate.rs`) **does**
stop the boot when a move cannot be made; its consolidation step checks the
checkpoint row too, instead of moving a journal a reader kept pages in.

### Reading a recipe without opening the bucket

`read_recipe_no_create(path)` reads `meta.embedding_recipe` without creating
or migrating the file (missing → `NotFound`, no `meta` table → `None`), and
`read_project_ids_no_create(path)` the project ids the same way (no `projects`
table → none).

Both wait at most the `busy` they are given for a writer — the daemon passes
`store.peek_busy_timeout_ms`. Both open read-write without `CREATE` and with
`query_only`: a read-only connection to a WAL file leaves `-wal`/`-shm` behind
on close, which is exactly what adoption reads as "held".

The boot scan peeks at every member through the first, so it never migrates the
files it peeks at.

### `buckets.toml` (`files/registry.rs`)

A discovery index for buckets whose location a name cannot derive. Two ways
in, on purpose:

- `Registry::load` — the boot's, **lenient**: a file that cannot be read costs
  its records for this run, with a warning, never the boot; a location whose
  family is provably gone (`family_absent`) is pruned in memory, and a base file
  renamed to its member keeps its entry. Nothing is written back.
- `Registry::read` — every other reader's, **strict**: a file that exists and
  cannot be read or parsed is a `Store` error naming it. `Registry::update`, the
  one locked read-modify-write (replaced atomically by write-beside + rename),
  reads through it and prunes nothing: a transient sharing violation or a bad
  hand edit is refused, never read as empty and written back over every
  tombstone.

It carries three more records nothing on disk could hold:

| Key | What |
|---|---|
| `detached.<project>` | `{ family, base, removing }` — a project taken off the daemon whose files stay; `removing` marks a `findery rm` in progress |
| `removing_families` | bucket names whose removal started and has not finished (tombstones) |
| `removing_members` | member file paths whose removal (`bucket rm --model`) started; the boot finishes them and never opens one meanwhile |

None is pruned, and a file written before they existed reads with them empty.
Malformed keys (not a project id, not a bucket name) are dropped with a
warning.

`findery detach` writes `detached.<project>`; `findery rm` and
`findery bucket rm` write their tombstone **before** their first destructive
step and clear it **after** their last, and the engine's boot finishes any it
finds before it opens a file (`.claude/docs/index/README.md` §5).

---

## 1. The bucket

### Schema (`bucket/schema.rs`, `user_version = 1`)

| Table        | Role                                                  |
|--------------|-------------------------------------------------------|
| `projects`   | definition + `last_sync_at`                           |
| `files`      | `(size, mtime_ns, content_hash)` — sync gates 1 and 2 |
| `chunks`     | the text, the lines, the symbol, the `content_hash`   |
| `embeddings` | **the cache**, keyed `(recipe, content_hash)`         |
| `vectors`    | live vectors for the `sqlite` backend                 |
| `chunks_fts` | FTS5, the BM25 leg of hybrid search                   |
| `meta`       | schema version, `embedding_recipe`                    |

`chunks` and `chunks_fts` are written **in the same transaction**
(CONV-027): a full-text index that lags returns hits pointing at text that's
gone, which is worse than having no BM25 leg at all.

### Concurrency

WAL + `synchronous = NORMAL`. A single `Mutex<Option<Connection>>`, every
method hops onto `spawn_blocking` through `with_conn`. It's the right
trade-off for a single-writer daemon: the queries are indexed lookups, and
**vector search never touches this connection**.

`Bucket::close()` ends the connection for **every clone at once**: it waits
for a transaction in flight (same lock), takes the connection out, runs a
checked `wal_checkpoint(TRUNCATE)` (an incomplete one is reported in the
`CloseReport`, and the close goes on), then `Connection::close()`.

Any clone used afterwards gets `NotFound("bucket `x` was closed")`; a second
close is a no-op. The OS lock on the file therefore no longer lasts as long as
the last `Arc` — `findery bucket rm` closes before it deletes, so a search or a
job still holding a handle cannot keep the file locked on Windows.

`Bucket::open_existing` opens a file that must exist (missing → `NotFound`,
nothing created), and still migrates it.

### The vector cache — the heart of the economics

Keyed `(recipe, content_hash)`, never the location. Consequences — and the
boundary of the claim, since `content_hash` covers the embedded text with its
`path:` header:

- re-running a sync on an unchanged repo: **zero embeddings**
- `sync --full` on an unchanged repo: **zero embeddings**
- re-cloning the repo to the same relative path: free
- reordering chunks inside a file: free
- moving a function to another file, renaming a file, renaming a module:
  **not** free — the path is part of the text that was embedded, and it is
  signal, not noise

`gc_embeddings()` exists but is **never** automatic: a cache collected eagerly
would defeat its own purpose, since the same fragment comes back the moment a
branch is switched back.

---

## 2. The `sqlite` backend — exact in-memory scan

Vectors are durable in the `vectors` table; search runs on a **contiguous
`f32` matrix in RAM**, built lazily, L2-normalized. Cosine = dot product.

The kernel uses eight independent accumulators to break the dependency
chain and let LLVM emit vectorized FMAs.

This is **not** "search with SQL". SQLite only handles persistence.

### The numbers — `make bench` (release, re-run on your own machine)

| Chunks  | Dim  | RAM      | Naive    | Shipped kernel |
|---------|------|----------|----------|----------------|
| 10,000  | 1024 | 41 MB    | 3.6 ms   | **2.4 ms**     |
| 50,000  | 1024 | 205 MB   | 18.2 ms  | **12.1 ms**    |
| 200,000 | 1024 | 819 MB   | 73.9 ms  | **50.1 ms**    |
| 10,000  | 2560 | 102 MB   | 9.1 ms   | **5.7 ms**     |
| 50,000  | 2560 | 512 MB   | 44.6 ms  | **28.1 ms**    |
| 200,000 | 2560 | 2,048 MB | 182.2 ms | **113.7 ms**   |

Order of magnitude: a ~500 k-line Rust workspace produces ~40–60 k chunks.

So we're **under 15 ms per query**, below the latency floor of the agent
asking the question, with **exact** recall and zero extra service.

### Why not an embedded ANN index

Also measured, with `hnsw_rs` (20,000 vectors, 1024 dims, release):

- search: **1.14 ms/query** — 4x faster than the scan, effectively
- build: **76 seconds** for 20,000 vectors

That's the wrong trade-off **here**. A code index mutates constantly: every
file save re-inserts.

Paying several milliseconds per insert to save a few milliseconds per query, on
a corpus that changes more often than it's queried, moves the cost to the wrong
place.

When the scan stops being enough, the right answer is an engine that handles
incremental insertion **and** ANN — that is, Qdrant.

---

## 3. The `qdrant` backend — the local vector database

A real vector engine, **local**, in the same `docker compose`:

```bash
FINDERY_STORE_BACKEND=qdrant docker compose --profile qdrant up -d
```

One collection per project and bucket file (prefix `findery_` by default):

- `<collection_prefix><project_id>` for a file with no namespace
- `<collection_prefix><namespace>.<project_id>` for one with

The `.` is in neither alphabet, so no namespace and project id (which may
contain `_`) can spell another pair's collection. `Cosine` distance, HNSW,
optional int8 quantization (`scalar_quantization = true`, 4x less RAM for ~1%
recall).

The namespace is the bucket file's `meta.vector_namespace`, read when the
bucket is opened and set with `Bucket::set_vector_namespace`
(`[a-z0-9_-]`, ≤ 32 characters). **Absent means empty**, so every file written
before members existed — adopted legacy files included — keeps the collections
it already has (`findery_<project>`).

A member the engine creates is stamped with its tag **before** its vector store
is built (`BucketFamily::ensure_member`), because two models of the same width
would otherwise write into one collection, and the width check could not tell
them apart.

**REST API, not the gRPC client**: the dependency is `reqwest`, already
present for the embedder, and our release cadence no longer keeps up with
that of a client library chasing the server's majors.

**Filters**:

- `lang` and `kind` are pushed server-side (with a `keyword` index).
- Path globs and symbol substrings have no Qdrant equivalent, so we over-fetch
  (×8, capped at 1024) and filter locally with `CompiledFilter` — the **same**
  semantics as the sqlite backend (CONV-011).

An approximate translation would be worse than a second pass over a few hundred
lines.

### Switching to it

```bash
docker compose --profile qdrant up -d qdrant   # the only container you need
findery daemon stop                            # the running one holds the old backend
export FINDERY_STORE_BACKEND=qdrant
export FINDERY_QDRANT_URL=http://127.0.0.1:6333
findery sync                                   # starts a daemon on the new backend
```

The `stop` is not optional: a daemon started earlier inherited the environment
of the command that started it, and exporting a variable afterwards leaves it
serving from SQLite.

The bucket does not move, so **the switch re-embeds nothing**: every vector is
already in the content-addressed cache and gets upserted into the new store.

The daemon notices on its own that the store is empty while the bucket is not,
upgrades the sync to a full pass and says so in the report. Without that, an
incremental pass would find every file unchanged, write nothing, and leave
search silently returning nothing.

### When to switch

| Signal                              | Backend                   |
|-------------------------------------|---------------------------|
| < ~150 k chunks, one machine        | `sqlite`                  |
| Measured latency > 50 ms            | `qdrant`                  |
| Daemon RAM is a problem             | `qdrant` (+ quantization) |
| Multiple machines on a shared index | `qdrant`                  |
| Exact recall required               | `sqlite`                  |

Switching = changing `FINDERY_STORE_BACKEND` and re-running a sync. The bucket
doesn't move, and since vectors are cached, **the sync re-embeds nothing**.

---

## 4. Export — never locked in

`export_project()` reads the **bucket** (chunks + cache), so it works
identically regardless of which backend is live.

| Format   | Files                                          | For                                  |
|----------|------------------------------------------------|--------------------------------------|
| `jsonl`  | `vectors.jsonl`                                | one-shot loading, human-readable     |
| `json`   | `vectors.json`                                 | a single array, convenient and heavy |
| `binary` | `vectors.f32` + `meta.jsonl` + `manifest.json` | 4 bytes/float, `numpy.fromfile`      |

```python
import json, numpy as np
m = json.load(open("manifest.json"))
V = np.fromfile("vectors.f32", dtype=np.float32).reshape(-1, m["dim"])
meta = [json.loads(l) for l in open("meta.jsonl", encoding="utf-8")]
assert len(meta) == V.shape[0]
```

A chunk with no vector under the current recipe is **counted** in the
report (`chunks` vs `vectors`), never invented or silently dropped.
