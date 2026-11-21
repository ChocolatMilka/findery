# `findery-server` — the HTTP API

A handler does three things: validate, call **one** `Indexer` method,
serialize (CONV-002). No decision lives here.

- **Base** — `http://127.0.0.1:8077` (`FINDERY_PORT`)
- **Content type** — `Content-Type: application/json`

## Authentication

Without `FINDERY_API_TOKEN`, everything is open — the single-user-machine case.

With it, every request carries `Authorization: Bearer <token>`, **except
`/healthz`**: a container health check that needed a secret would be a
health check that ends up disabled.

The comparison is constant-time.

---

## Errors

Uniform body, status derived from `Error::http_status()` (CONV-010):

```json
{ "code": "not_found", "message": "project `axle`" }
```

`code` names the variant, `message` is its **payload** — never the rendered
error.

A client that branches on `code` rebuilds the variant and renders it once; a
server that sent `Error::to_string()` here would make every message read
``not found: not found: project `axle` ``.

| `code`                    | Status | Meaning                                    |
|---------------------------|--------|--------------------------------------------|
| `invalid`, `config`       | 400    | input rejected before any side effect      |
| —                         | 401    | missing or wrong token                     |
| `not_found`               | 404    | unknown project, job, or route             |
| `upstream`, `embed`       | 502    | the embeddings runtime or Qdrant went down |
| `store`, `io`, `internal` | 500    | our fault                                  |

`findery-client` rebuilds the typed error from `code`: a caller does a `match`
on `Error::NotFound`, not on an integer it had to go look up.

---

## Routes

| Method           | Path                                | What it is for                                   |
|------------------|-------------------------------------|--------------------------------------------------|
| `GET`            | `/healthz`                          | liveness only, no auth                           |
| `GET`            | `/v1/health`                        | readiness: probes the embeddings runtime         |
| `POST`           | `/v1/shutdown`                      | asks the daemon to stop, gracefully              |
| `GET`, `POST`    | `/v1/projects`                      | the projects; `POST` registers one               |
| `GET`, `DELETE`  | `/v1/projects/{id}`                 | one project; `DELETE` removes it                 |
| `GET`            | `/v1/projects/{id}/stats`           | counters, model, backend, last sync, bucket size |
| `GET`, `POST`    | `/v1/buckets`                       | the independent indexes the daemon serves        |
| `DELETE`         | `/v1/buckets/{name}`                | removes a bucket with its projects and vectors   |
| `POST`           | `/v1/projects/{id}/sync`            | a sync, as a job or blocking                     |
| `POST`, `DELETE` | `/v1/projects/{id}/documents`       | one document, with or without its text           |
| `POST`           | `/v1/projects/{id}/watch`           | the project's watcher                            |
| `POST`           | `/v1/projects/{id}/export`          | an export to a host directory                    |
| `POST`           | `/v1/search`                        | hybrid search                                    |
| `GET`            | `/v1/projects/{id}/files`           | the manifest a pushing client diffs against      |
| `POST`           | `/v1/projects/{id}/documents/batch` | documents a client walked and read itself        |
| `POST`           | `/v1/projects/{id}/prune`           | deletes pushed files outside the `keep` list     |
| `GET`            | `/v1/jobs`, `/v1/jobs/{id}`         | jobs, with `progress` while running              |

### `GET /healthz`

Liveness only.

- **Auth** — none
- **Dependency probe** — none
- **Response** — `200 ok`

This is what a client probes before starting a daemon: it answers even while the
embedding runtime is down, so a degraded daemon is never mistaken for an absent
one and started a second time.

### `GET /v1/health`

Readiness: probes the embeddings runtime.

**Response**

```json
{ "ok": true, "model": "qwen3.7-text-embedding", "dim": 1024,
  "store_backend": "sqlite", "projects": 3, "embedder_ok": true,
  "version": "0.1.1" }
```

`version` is the version of the **daemon binary** that answered. It is what lets
a client tell an outdated daemon from a broken one:

- a daemon that predates a route answers 404 on it
- a daemon that predates this field answers without it

`findery doctor` reads both as "rebuild and restart me".

`ok: false` with `embedder_ok: false` and a `detail` string is the normal answer
when the model is not up. The daemon still serves everything that does not need
a vector — see `.claude/docs/embed/README.md` on degraded start.

### `POST /v1/shutdown`

Asks the daemon to stop, gracefully: it stops accepting connections, finishes
what is in flight, and exits.

**Response**

```json
{ "stopping": true }
```

This is also what `findery config set` uses to make a written setting take
effect: the daemon read its configuration at boot, so the only way to apply a
change is to let it be replaced.

INTENT: a daemon that started itself has no terminal to Ctrl-C and no pid anyone
wrote down. Its own API is the only channel that works identically on Windows,
macOS and Linux.

It requires the bearer token like every other route — a daemon anyone on the
network can stop is not a daemon.

### `GET /v1/projects` · `POST /v1/projects`

**Request** (`POST`)

```jsonc
// POST — `root` is a HOST path, translated by the mounts
{ "root": "C:/Users/users/RustroverProjects/axle",
  "id": "axle",                       // optional, otherwise derived from the name
  "extensions": ["rs", "md", "py"],   // optional; defaults to every known language
  "exclude": ["**/generated/**"],     // optional
  "bucket": { "name": "work" },       // optional; see below
  "scopes": { "api": { "paths": ["crates/server/**"] } },  // optional
  "instruction": "Given …",           // optional; this project's default intent
  "follow_gitignore": true }
```

Idempotent on `id`: re-posting will update the definition without losing
the index.

#### How `bucket` resolves

`bucket` is read at registration only, and it is a **reference**, not a name:

| Sent                                            | Resolved to                                                     |
|-------------------------------------------------|-----------------------------------------------------------------|
| absent                                          | a bucket inside the project, named after it                     |
| `{"name": "work"}`                              | `<data_dir>/buckets/work/bucket.db`, shared by whoever names it |
| `{"path": "D:/shared/team.db"}`                 | that file. Relative paths resolve against the project root      |
| `{"name": "team", "path": "D:/shared/team.db"}` | that file, under that name                                      |

Every bucket ends up with a name, even one described only by a path: two
projects pointing at one file must reach *one* bucket, so the name is derived
from the path when none was given.

The resolution needs to know whether the daemon can write inside the project,
which it decides the same way it decides pull versus push.

A root it cannot reach — a container, another machine — makes a project-local
bucket impossible. The request is honoured as far as it can be, and the daemon
answers with a bucket named after the project.

> **Note** — the answer, never the request, is what a client should record:
> `Project.bucket` is carried back on every later read, so a client cannot
> disagree with the daemon about it.

### `GET | DELETE /v1/projects/{id}`

- **`DELETE`** — removes the project, its chunks and its vectors
- **Response** (`DELETE`) — `{"deleted": true}`

### `GET /v1/projects/{id}/stats`

Counters, model, backend, last sync, bucket size.

### `GET · POST /v1/buckets` · `DELETE /v1/buckets/{name}`

The independent indexes the daemon serves.

#### `GET`

Lists them with what each holds:

```json
[{ "name": "default", "path": "…/bucket.db", "recipe": "qwen3.7-text-embedding:1024",
   "conflict": null, "projects": 3, "chunks": 41207, "disk_bytes": 138412032,
   "is_default": true }]
```

`conflict` is the repair message when a bucket holds vectors built by another
model, and `null` when it agrees with the daemon.

It is reported here rather than raised, so a bucket in conflict is still listed,
still deletable, and still serving keyword search — see
`.claude/docs/embed/README.md`.

#### `POST`

Takes `{"name": "work"}` and creates one, or reports the one already there.

#### `DELETE`

Removes it **with its projects and vectors**:

```json
{ "removed": true, "projects_removed": 2, "projects": ["axle", "smalt"] }
```

Vectors are dropped before the file is removed: a bucket row with no vectors
self-heals on the next sync, an orphaned collection does not.

It goes through the daemon because the daemon holds the file open, and a SQLite
file a live process has open cannot be removed — on Windows the unlink fails
outright. The daemon drops its handle first, which is the one thing a CLI
deleting the file directly could not do.

The **default** bucket is refused: `default` is the one every install has, and
the daemon recreates it at boot, so removing it would be a no-op reporting
success. Delete its projects instead.

### `POST /v1/projects/{id}/sync`

**Request**

```jsonc
{ "mode": "incremental",       // "incremental" (default) | "full"
  "paths": ["src/net.rs"],     // implies targeted mode; no deletions
  "wait": false }              // true → blocks and returns the report
```

**Response**

- `wait: false` returns `{"job": {...}}`
- `wait: true` returns `{"report": {...}}`

A sync already active on the project is returned as-is instead of stacking a
second one.

### `POST | DELETE /v1/projects/{id}/documents`

**Request** (`POST`)

```jsonc
// POST — without `text`, the file is read from disk
{ "path": "notes/decisions.md", "text": "# Decisions\n\n…" }
```

A document pushed with its text is `Origin::Inline`: the sync scan will never
delete it.

**Remove** — `DELETE ?path=notes/decisions.md` → `{"removed": 3}`.

### `POST /v1/projects/{id}/watch`

**Request** — `{"enabled": true, "debounce_ms": 1500}`.

Re-posting replaces the existing watcher.

### `POST /v1/projects/{id}/export`

**Request** — `{"format": "jsonl" | "json" | "binary", "dir": "C:/exports"}`.

`dir` is a host path, translated by the mounts.

### `POST /v1/search`

**Request**

```jsonc
{ "project": "axle",
  "query": "where is retry backoff handled",
  "k": 8,
  "mode": "hybrid",                 // "vector" | "keyword" | "hybrid"
  "task": "symbol",                 // retrieval intent — see embed/README.md
  "instruction": "Given …",         // free-form, overrides `task` and the project's
  "scope": "api",                   // a restriction the project declared, by name
  "filter": { "paths": ["crates/**"], "langs": ["rust"],
              "kinds": ["function", "method"], "symbol_contains": "retry" },
  "max_snippet_chars": 1200 }
```

`scope` is a **name**, and the daemon is what turns it into a filter — it holds
the project's definition, and the MCP bridge is a different process that never
reads a manifest.

The scope is layered per dimension: a `filter` the request supplies replaces the
scope's value for the dimensions it names, and keeps the scope's for the rest.

`instruction` and `query` are the two halves of what the model reads
(`Instruct: …` / `Query: …`). Both are always sent; the precedence is
`instruction` > `task` > the project's manifest > the daemon's configured
default.

`task` is one of `code symbol usage docs test config error`. That precedence
is resolved in L0, so no transport can invent its own ordering.

**Errors**

- an unknown `scope` name is a 400 that lists the ones that exist
- an unknown `task` value is rejected rather than ignored

**Response** — an array of `SearchHit`, descending.

```jsonc
[{ "chunk": { "path": "crates/net/src/retry.rs", "start_line": 12, "end_line": 48,
              "kind": "method", "breadcrumb": "Client > retry_with_backoff",
              "lang": "rust", "text": "…" },
   "score": 0.031, "vector_score": 0.842, "keyword_score": 7.1,
   "host_path": "C:/Users/users/RustroverProjects/axle/crates/net/src/retry.rs" }]
```

`host_path` is absolute **on the host side**: a hit that cites a container
path is a hit nobody can open.

### `GET /v1/projects/{id}/files`

The manifest a pushing client diffs against: every known file with its
`content_hash`, `size`, `origin` and chunk count.

Without it a client would have to send the whole project on every sync.

### `POST /v1/projects/{id}/documents/batch`

**Request**

```jsonc
{ "documents": [ { "path": "src/lib.rs", "text": "…" }, … ] }
```

Ingests documents the client walked and read itself, in one round trip.

- Files arrive as `origin: "pushed"`.
- Re-sending identical content is free — the hash is compared before anything
  is chunked.

**Response** — a `SyncReport`.

### `POST /v1/projects/{id}/prune`

**Request**

```jsonc
{ "keep": ["src/lib.rs", "README.md", …] }
```

Every path the client still has. Pushed files outside that list are deleted.

> **Note** — **only `origin: "pushed"` rows are touched.** A client can never
> delete what the daemon walked, nor a hand-added inline document. Each side
> prunes what it fed — otherwise one client's sync would silently erase
> another feeder's files.

### `GET /v1/jobs` · `GET /v1/jobs/{id}`

A **running** job carries `progress`, filled at the moment it is read:

```jsonc
{ "id": "a3f1…", "project": "axle", "kind": "full", "state": "running",
  "progress": { "phase": "indexing", "files_done": 412, "files_total": 2048,
                "chunks_done": 3180, "embeddings_computed": 900,
                "embeddings_cached": 2280, "elapsed_ms": 4100 } }
```

`files_total` comes from the walk, which runs before anything is read — so the
proportion is a real one and not a guess.

The field is absent:

- on a queued job — it has not started
- on a terminal one — its `report` is the description

That is why a client must treat `progress` as optional in both directions.

---

## Limits

- `max_body_bytes` (32 MiB) caps request bodies.
- An unknown route returns 404, not 405, so a typo doesn't look like a
  forbidden method.

---

## Verifying

`crates/server/tests/api.rs` exercises the full stack — router, engine,
chunkers, bucket, store — with only the embedder swapped for the mock,
because a model is the one thing a test can't host.

Thirteen cases, including the proof that the second sync computes zero
embeddings.
