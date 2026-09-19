# `findery-server` — the HTTP API

A handler does three things: validate, call **one** `Indexer` method,
serialize (CONV-002). No decision lives here.

Base: `http://127.0.0.1:8077` (`FINDERY_PORT`). `Content-Type: application/json`.

## Authentication

Without `FINDERY_API_TOKEN`, everything is open — the single-user-machine case.
With it, every request carries `Authorization: Bearer <token>`, **except
`/healthz`**: a container health check that needed a secret would be a
health check that ends up disabled. The comparison is constant-time.

## Errors

Uniform body, status derived from `Error::http_status()` (CONV-010):

```json
{ "code": "not_found", "message": "not found: project `axle`" }
```

| `code` | Status | Meaning |
|--------|--------|------|
| `invalid`, `config` | 400 | input rejected before any side effect |
| — | 401 | missing or wrong token |
| `not_found` | 404 | unknown project, job, or route |
| `upstream`, `embed` | 502 | the embeddings runtime or Qdrant went down |
| `store`, `io`, `internal` | 500 | our fault |

`findery-client` rebuilds the typed error from `code`: a caller does a `match`
on `Error::NotFound`, not on an integer it had to go look up.

---

## Routes

### `GET /healthz`
Liveness only. No auth, no dependency probe. `200 ok`.

This is what a client probes before starting a daemon: it answers even while the
embedding runtime is down, so a degraded daemon is never mistaken for an absent
one and started a second time.

### `GET /v1/health`
Readiness: probes the embeddings runtime.

```json
{ "ok": true, "model": "qwen3.7-text-embedding", "dim": 1024,
  "store_backend": "sqlite", "projects": 3, "embedder_ok": true }
```

`ok: false` with `embedder_ok: false` and a `detail` string is the normal answer
when the model is not up. The daemon still serves everything that does not need
a vector — see `.claude/docs/embed/README.md` on degraded start.

### `POST /v1/shutdown`
Asks the daemon to stop, gracefully: it stops accepting connections, finishes
what is in flight, and exits.

```json
{ "stopping": true }
```

INTENT: a daemon that started itself has no terminal to Ctrl-C and no pid anyone
wrote down. Its own API is the only channel that works identically on Windows,
macOS and Linux. It requires the bearer token like every other route — a daemon
anyone on the network can stop is not a daemon.

### `GET /v1/projects` · `POST /v1/projects`

```jsonc
// POST — `root` is a HOST path, translated by the mounts
{ "root": "C:/Users/users/RustroverProjects/axle",
  "id": "axle",                       // optional, otherwise derived from the name
  "extensions": ["rs", "md"],         // optional
  "exclude": ["**/generated/**"],     // optional
  "follow_gitignore": true }
```

Idempotent on `id`: re-posting will update the definition without losing
the index.

### `GET | DELETE /v1/projects/{id}`
`DELETE` removes the project, its chunks and its vectors → `{"deleted":
true}`.

### `GET /v1/projects/{id}/stats`
Counters, model, backend, last sync, catalog size.

### `POST /v1/projects/{id}/sync`

```jsonc
{ "mode": "incremental",       // "incremental" (default) | "full"
  "paths": ["src/net.rs"],     // implies targeted mode; no deletions
  "wait": false }              // true → blocks and returns the report
```

`wait: false` returns `{"job": {...}}`; `wait: true` returns `{"report":
{...}}`. A sync already active on the project is returned as-is instead of
stacking a second one.

### `POST | DELETE /v1/projects/{id}/documents`

```jsonc
// POST — without `text`, the file is read from disk
{ "path": "notes/decisions.md", "text": "# Decisions\n\n…" }
```

A document pushed with its text is `Origin::Inline`: the sync scan will
never delete it. `DELETE ?path=notes/decisions.md` → `{"removed": 3}`.

### `POST /v1/projects/{id}/watch`
`{"enabled": true, "debounce_ms": 1500}`. Re-posting replaces the existing
watcher.

### `POST /v1/projects/{id}/export`
`{"format": "jsonl" | "json" | "binary", "dir": "C:/exports"}` — `dir` is a
host path, translated by the mounts.

### `POST /v1/search`

```jsonc
{ "project": "axle",
  "query": "where is retry backoff handled",
  "k": 8,
  "mode": "hybrid",                 // "vector" | "keyword" | "hybrid"
  "task": "symbol",                 // retrieval intent — see embed/README.md
  "instruction": "Given …",         // free-form, overrides `task`
  "filter": { "paths": ["crates/**"], "langs": ["rust"],
              "kinds": ["function", "method"], "symbol_contains": "retry" },
  "max_snippet_chars": 1200 }
```

Response: an array of `SearchHit`, descending.

```jsonc
[{ "chunk": { "path": "crates/net/src/retry.rs", "start_line": 12, "end_line": 48,
              "kind": "method", "breadcrumb": "Client > retry_with_backoff",
              "lang": "rust", "text": "…" },
   "score": 0.031, "vector_score": 0.842, "keyword_score": 7.1,
   "host_path": "C:/Users/users/RustroverProjects/axle/crates/net/src/retry.rs" }]
```

`task` is one of `code symbol usage docs test config error`; an unknown value is
rejected rather than ignored. Precedence — `instruction` > `task` > the daemon's
configured default — is resolved in L0, so no transport can invent its own
ordering.

`host_path` is absolute **on the host side**: a hit that cites a container
path is a hit nobody can open.

### `GET /v1/projects/{id}/files`

The manifest a pushing client diffs against: every known file with its
`content_hash`, `size`, `origin` and chunk count. Without it a client would have
to send the whole project on every sync.

### `POST /v1/projects/{id}/documents/batch`

```jsonc
{ "documents": [ { "path": "src/lib.rs", "text": "…" }, … ] }
```

Ingests documents the client walked and read itself, in one round trip. Files
arrive as `origin: "pushed"`. Re-sending identical content is free — the hash is
compared before anything is chunked. Returns a `SyncReport`.

### `POST /v1/projects/{id}/prune`

```jsonc
{ "keep": ["src/lib.rs", "README.md", …] }
```

Every path the client still has. Pushed files outside that list are deleted.

**Only `origin: "pushed"` rows are touched.** A client can never delete what the
daemon walked, nor a hand-added inline document. Each side prunes what it fed —
otherwise one client's sync would silently erase another feeder's files.

### `GET /v1/jobs` · `GET /v1/jobs/{id}`

---

## Limits

`max_body_bytes` (32 MiB) caps request bodies. An unknown route returns
404, not 405, so a typo doesn't look like a forbidden method.

## Verifying

`crates/server/tests/api.rs` exercises the full stack — router, engine,
chunkers, catalog, store — with only the embedder swapped for the mock,
because a model is the one thing a test can't host. Thirteen cases,
including the proof that the second sync computes zero embeddings.
