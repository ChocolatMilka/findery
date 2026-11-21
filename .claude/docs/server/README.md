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
error. A client that branches on `code` rebuilds the variant and renders it
once; a server that sent `Error::to_string()` here would make every message
read ``not found: not found: project `axle` ``.

| `code` | Status | Meaning |
|--------|--------|------|
| `invalid`, `config` | 400 | input rejected before any side effect (a bad bucket name is `invalid`) |
| `model_unknown` | 400 | a model name no profile defines; the message lists the known ones and where to add one |
| — | 401 | missing or wrong token |
| `not_found` | 404 | unknown project, job, or route |
| `recipe_mismatch` | 409 | the bucket's vectors belong to another embedding space (another `<model>:<dim>`, or another width); the message names the repair |
| `vectors_unavailable` | 409 | the project has no vectors under the served recipe yet; sync it, keyword search works meanwhile |
| `busy` | 409 | a running job or another process holds what the operation needs; nothing was changed |
| `cancelled` | 409 | the operation was stopped on purpose (the message names by what) |
| `upstream`, `embed` | 502 | the embeddings runtime or Qdrant went down |
| `store`, `io`, `internal` | 500 | our fault |

A 409 is a conflict the **user** resolves — re-index, sync, wait — so it is
neither a 400 (nothing in the request was malformed) nor a 500 (nothing is
broken). Each of those messages is written once, by a constructor on `Error`
(`Error::recipe_mismatch`, `Error::width_mismatch`, …), and names the exact
command that repairs it.

`findery-client` rebuilds the typed error from `code` through
`Error::from_wire`, the inverse of `Error::code()` kept beside it in L0 and
round-trip tested for every variant: a caller does a `match` on
`Error::NotFound`, not on an integer it had to go look up.

A code the client does not know (a newer daemon) becomes `internal` with the
message intact. An `upstream` payload carries its service
(`"embedder: connection refused"`), so a down embedder is never reported as a
down daemon.

---

## Routes

| Method           | Path                                | What it is for                                   |
|------------------|-------------------------------------|--------------------------------------------------|
| `GET`            | `/healthz`                          | liveness only, no auth                           |
| `GET`            | `/v1/health`                        | readiness: probes the embeddings runtime         |
| `POST`           | `/v1/shutdown`                      | asks the daemon to stop, gracefully              |
| `GET`, `POST`    | `/v1/projects`                      | the projects; `POST` registers one               |
| `GET`, `DELETE`  | `/v1/projects/{id}`                 | one project; `DELETE` removes it                 |
| `POST`           | `/v1/projects/{id}/detach`          | takes a project off, keeps its index on disk     |
| `GET`            | `/v1/models`                        | the model profiles, without probing anything     |
| `POST`           | `/v1/models/default`                | the model of every project without a pin         |
| `GET`            | `/v1/projects/{id}/stats`           | counters, model, backend, last sync, bucket size |
| `GET`, `POST`    | `/v1/buckets`                       | the independent indexes the daemon serves        |
| `DELETE`         | `/v1/buckets/{name}`                | removes a bucket with its projects and vectors   |
| `DELETE`         | `/v1/buckets/{name}?model=<alias>`  | removes one model's file of the bucket           |
| `POST`           | `/v1/projects/{id}/sync`            | a sync, as a job or blocking                     |
| `POST`, `DELETE` | `/v1/projects/{id}/documents`       | one document, with or without its text           |
| `POST`           | `/v1/projects/{id}/watch`           | the project's watcher                            |
| `POST`           | `/v1/projects/{id}/export`          | an export to a host directory                    |
| `POST`           | `/v1/search`                        | hybrid search                                    |
| `GET`            | `/v1/intents`                       | the intent library                               |
| `GET`            | `/v1/projects/{id}/files`           | the manifest a pushing client diffs against      |
| `GET`            | `/v1/projects/{id}/chunks`          | the lines a locator or a chunk id names          |
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

Readiness: probes the embeddings runtimes already connected.

**Response**

```jsonc
{ "ok": true, "model": "qwen3.7-text-embedding", "dim": 1024,
  "store_backend": "sqlite", "projects": 3, "embedder_ok": true,
  "version": "0.4.0",
  "notices": ["bucket `axle`: …/.findery/bucket.db is now …/.findery/bucket.text-embedding-qwen3-embedding-0-6b-47m32eui.db (`…:1024`)"],
  "models": [
    { "alias": "embedder", "model": "qwen3.7-text-embedding", "dim": 1024,
      "recipe": "qwen3.7-text-embedding:1024", "is_default": true,
      "connected": true, "ok": true },
    { "alias": "harrier", "model": "harrier-oss-v1-0.6b", "dim": 1024,
      "recipe": "harrier-oss-v1-0.6b:1024", "is_default": false,
      "connected": false, "ok": false,
      "detail": "not used yet: built on the first operation that needs it" } ] }
```

The top-level `model`, `dim` and `embedder_ok` describe the **default** profile,
for clients that predate `models`.

A profile other than the default is built on first use, and `health` never
builds one to probe it: `connected: false` is not a failure, and a profile
whose last build failed says why in `detail`.

`notices` carries what the daemon did, or could not do, on its own:

- a legacy bucket file adopted or kept in place (and why)
- a project id found in two buckets (and which one serves)
- a removal the boot could not finish

Both arrays are omitted when empty.

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

The projects; `POST` registers one.

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
  "model": "harrier",                 // optional; pins the project to a model profile
  "follow_gitignore": true }
```

Idempotent on `id` — and on `root`: a request with no `id` reuses the id of
the project already attached at that root. Re-posting updates the definition
without losing the index, in every model's file of the bucket, so scopes and
the pin never diverge between models.

A re-post is a **merge**: every field it leaves absent keeps the project's
current value — `bucket` included, so a re-post never moves a project it does
not name a bucket for — and only an explicitly empty value clears one:

- `[]` for `include`/`exclude`/`extensions`
- `{}` for `scopes`/`intents`
- `""` for `instruction` and `model` (unpinning)

`{"root": "…"}` alone, which is what the MCP bridge and most scripts send,
changes nothing the manifest declared.

Re-posting a **detached** project attaches it again, instantly: its files are
reopened where `detach` left them.

With `model` absent, the project follows the daemon's default model
(`POST /v1/models/default`); every `Project` read back carries `model`, the
profile it uses right now.

**Errors** — `model` is validated before anything is written: an alias no
profile defines is `400 model_unknown`, naming the known ones.

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

### `GET /v1/projects/{id}` · `DELETE /v1/projects/{id}`

One project; `DELETE` removes it.

`DELETE` is `findery rm`: it removes the project, its chunks and its vectors
from every model's file of its bucket — and the files themselves when no other
project, attached or detached, shares the bucket (never for `default`).

The project's running sync is stopped first and its watcher dropped (see
`.claude/docs/index/README.md` §5). A detached project is removed the same way.

**Response** (`DELETE`)

```json
{ "project": "axle", "deleted": true, "rows_only": false,
  "files_deleted": ["C:/…/axle/.findery/bucket.harrier-oss-v1-0-6b-zvpmtsdf.db"] }
```

`deleted` stays for clients that read the older `{"deleted": true}`, and the
other way round every other field has a default, so `findery-client` reads an
older daemon's `{"deleted": true}` as a removal that names no file.

A shared bucket answers `"rows_only": true` with `shared_with`. The project's
`.findery/config.toml` is kept.

**Errors**

| Case | Answer |
|---|---|
| a file another process holds | `409 busy`; the removal is recorded as a tombstone and finished by the next start, before anything opens the file |
| a sync that does not stop within `server.drain_timeout_secs` | `409 busy`, and nothing is changed |

### `POST /v1/projects/{id}/detach`

Takes the project off the daemon and **keeps** its index on disk.

Its sync is stopped (the job ends `failed` with code `cancelled`), its watcher
dropped, and when no attached project is left in its bucket (and it is not
`default`) every file of the bucket is closed: unlocked, movable, and not
reopened at the next start.

**Response**

```json
{ "project": "axle", "closed": ["C:/…/axle/.findery/bucket.harrier-oss-v1-0-6b-zvpmtsdf.db"] }
```

A bucket that stays open says why in `still_open` (`"shared with `smalt`"`).
`POST /v1/projects` with the same root attaches it again, instantly.

**Errors** — the same `409 busy` rule as `DELETE`.

### `GET /v1/models` · `POST /v1/models/default`

The model profiles, and which one every project without a pin uses.

**Response** (`GET`) — `GET` lists the model profiles (`[embedder]` as
`embedder`, then each `[models.<alias>]`) without probing anything — the
`models` entries of `/v1/health` without `ok`/`detail`.

**Request** (`POST`) — `{"alias": "harrier"}`.

`POST` makes that profile the model of every project without a pin, **from the
next operation on**: nothing is rebuilt or moved, and switching back costs
nothing. The switch lasts until the daemon restarts — `default_model` in the
configuration is what the next start uses.

**Response** (`POST`) — the profile.

**Errors** — an unknown alias is `400 model_unknown` and changes nothing.

`findery model use` does both: this call, then `default_model` saved in
`<data_dir>/config.toml`, without restarting anything.

### `GET /v1/projects/{id}/stats`

Counters, model, backend, last sync, bucket size, and the language mix.

**Response** — the language mix is
`"langs": [{ "lang": "rust", "chunks": 812 }, …]`, most chunks first, absent
when nothing is indexed under the project's model.

### `GET /v1/buckets` · `POST /v1/buckets` · `DELETE /v1/buckets/{name}[?model=<alias>]`

The independent indexes the daemon serves.

#### `GET`

Lists them with what each holds.

**Response**

```jsonc
[{ "name": "axle", "path": "D:/work/axle/.findery/bucket.db",
   "recipe": "harrier-oss-v1-0.6b:1024", "projects": 1, "chunks": 812,
   "disk_bytes": 6000000, "is_default": false,
   "members": [
     { "path": "D:/work/axle/.findery/bucket.harrier-oss-v1-0-6b-zvpmtsdf.db",
       "tag": "zvpmtsdf", "recipe": "harrier-oss-v1-0.6b:1024", "legacy": false,
       "serving": true, "projects": 1, "chunks": 812, "disk_bytes": 3100000 },
     { "path": "D:/work/axle/.findery/bucket.db", "tag": "47m32eui",
       "recipe": "text-embedding-qwen3-embedding-0.6b:1024", "legacy": true,
       "serving": false, "projects": 1, "chunks": 790, "disk_bytes": 2900000 } ] }]
```

For the bucket as a whole:

| Field | Means |
|---|---|
| `path` | the bucket's **base** path — the name manifests and `buckets.toml` use; the files on disk are its `members`, one per model (`.claude/docs/store/README.md` §0) |
| `recipe`, `conflict`, `chunks` | describe the member of the default model (absent when the bucket has none yet) |
| `projects` | counts every project with a row in any member |
| `disk_bytes` | sums every member |

Each member says:

| Field | Means |
|---|---|
| `path` | which file it is |
| `tag` | the tag it is found under |
| `recipe` | the recipe it holds |
| `legacy` | whether it is the base file adoption could not rename yet |
| `serving` | whether it is the default model's file |
| `projects`, `chunks`, `disk_bytes` | its own counts |
| `conflict` | its conflict, when it has one |

`members` is absent from a daemon that predates per-model files; a client then
lists the bucket as a whole.

`conflict` is the repair message when that member holds vectors built by
another model than its name says, and `null` when it agrees. It is reported
here rather than raised, so a bucket in conflict is still listed, still
deletable, and still serving keyword search — see
`.claude/docs/embed/README.md`.

#### `POST`

**Request** — `{"name": "work"}`.

Creates the bucket — its file for the default model — or reports the one
already there.

#### `DELETE`

Removes the bucket **with its projects and vectors**, every model's file of it.

**Response**

```json
{ "removed": true, "projects_removed": 2, "projects": ["axle", "smalt"] }
```

Vectors are dropped before the file is removed: a bucket row with no vectors
self-heals on the next sync, an orphaned collection does not.

It goes through the daemon because the daemon holds the file open, and a SQLite
file a live process has open cannot be removed — on Windows the unlink fails
outright. The daemon:

1. stops the syncs and watchers of the bucket's projects
2. writes a **tombstone** in `buckets.toml`
3. **closes** every member, which releases the OS lock even while a search or
   a job still holds a reference to one — the one thing a CLI deleting the
   file directly could not do
4. deletes each file set (database, `-wal`, `-shm`)
5. forgets the `buckets.toml` entry and the tombstone **last**, so a delete
   that fails leaves both, and the next start finishes it before opening
   anything

**Errors**

| Case | Answer |
|---|---|
| a file another process still holds | `409 busy`, naming the file and the command to run again |
| the **default** bucket | refused |

`default` is refused because it is the one every install has, and the daemon
recreates it at boot, so removing it would be a no-op reporting success.
Delete its projects instead.

#### `DELETE ?model=<alias>`

`DELETE /v1/buckets/{name}?model=<alias>` removes **one model's file** of the
bucket — `default` included — and nothing else: the projects keep their rows
in the other members, and their syncs are stopped and released, not ended.

**Response**

```json
{ "removed": true, "projects_removed": 0, "projects": [] }
```

**Errors**

| Case | Answer |
|---|---|
| an alias no profile defines | `400 model_unknown`, naming the known ones |
| the bucket has no file for that model | `404 not_found` |
| it is the bucket's last file | `400 invalid`, naming `findery bucket rm <name>` |
| a misspelled parameter (`?modle=`) | `400`, nothing removed — never a fall-through to the whole bucket |

### `POST /v1/projects/{id}/sync`

A sync, as a job or blocking.

**Request**

```jsonc
{ "mode": "incremental",       // "incremental" (default) | "full"
  "paths": ["src/net.rs"],     // implies targeted mode; no deletions
  "wait": false }              // true → blocks and returns the report
```

**Response**

- `wait: false` — returns `{"job": {...}}`
- `wait: true` — returns `{"report": {...}}`
- `"wait_ms": 20000` — the bounded middle: the job is started (or the running
  one joined) and waited for at most that long — capped by
  `server.max_sync_wait_ms` (120 s) — then answered as `{"report": …}` when it
  ended in time, as the job's own error when it failed, and as `{"job": …}`
  with its live `progress` otherwise. It is what the MCP `sync` tool sends.

A sync already active on the project is returned as-is instead of stacking a
second one. The job is spawned by the engine (`Indexer::start_sync`), not by
the handler, so `detach` and `rm` can find it and stop it.

The first sync under a model creates that model's file of the bucket and
embeds from zero; switching back to a model already synced embeds nothing.

### `POST /v1/projects/{id}/documents` · `DELETE /v1/projects/{id}/documents`

One document, with or without its text.

**Request** (`POST`)

```jsonc
// POST — without `text`, the file is read from disk
{ "path": "notes/decisions.md", "text": "# Decisions\n\n…" }
```

A document pushed with its text is `Origin::Inline`: the sync scan will never
delete it.

**Request** (`DELETE`) — `?path=notes/decisions.md` removes the document.

**Response** (`DELETE`) — `{"removed": 3}`.

### `POST /v1/projects/{id}/watch`

The project's watcher.

**Request** — `{"enabled": true, "debounce_ms": 1500}`.

Re-posting replaces the existing watcher. The engine owns the watchers
(`Indexer::set_watch`); a watcher's syncs are jobs, deferred while another job
runs on the project, and a watcher whose project is gone stops.

### `POST /v1/projects/{id}/export`

An export to a host directory.

**Request** — `{"format": "jsonl" | "json" | "binary", "dir": "C:/exports"}`.

`dir` is a host path, translated by the mounts.

### `POST /v1/search`

Hybrid search.

**Request**

```jsonc
{ "project": "axle",
  "query": "where is retry backoff handled",
  "k": 8,                           // per list; absent: search.default_k, or each leg's own
  "mode": "hybrid",                 // "vector" | "keyword" | "hybrid"
  "intent": "usage",                // a library intent, by name or alias — see embed/README.md
  "task": "symbol",                 // legacy intent name; same namespace as `intent`
  "instruction": "Given …",         // free-form, replaces the intent's sentence
  "raw_query": false,               // true: the query verbatim, no instruction, no template
  "like": "src/net.rs:L12-L48",     // a chunk (or chunk id): search by its stored vector
  "scope": "api",                   // a restriction the project declared, by name
  "filter": { "paths": ["crates/**"], "langs": ["rust"],
              "kinds": ["function", "method"], "symbol_contains": "retry" },
  "max_snippet_chars": 1200 }
```

`scope` is a **name**, and the daemon is what turns it into a filter — it holds
the project's definition; a client reads a manifest only to learn which project
it stands in, never to expand a scope.

The scope is layered per dimension: a `filter` the request supplies replaces
the scope's value for the dimensions it names, and keeps the scope's for the
rest.

`instruction` and `query` are the two halves of what the model reads
(`Instruct: …` / `Query: …`). The precedence is `raw_query` > `instruction` >
`intent` / `task` > `like` (runs `similar`) > the project's manifest > the
daemon's configured default, resolved in L0 (`SearchRequest::resolve_plan`), so
no transport can invent its own ordering.

- `task` is one of `code symbol usage docs test config error` (or an alias).
- `intent` is any intent of the library.
- `like` is a locator (`path:L12-L48`, `path:12-48`, `path:L12`) or a chunk id.
  It embeds nothing; the chunk itself is never in the answer.
- The body is `deny_unknown_fields`.

**Response** — a `SearchResponse`.

```jsonc
{ "hits": [{ "chunk": { "id": "…", "path": "crates/net/src/retry.rs",
                        "start_line": 12, "end_line": 48, "kind": "method",
                        "breadcrumb": "Client > retry_with_backoff", "lang": "rust", "text": "…" },
             "score": 0.031, "vector_score": 0.842, "keyword_score": 7.1,
             "host_path": "C:/Users/users/RustroverProjects/axle/crates/net/src/retry.rs" }],
  "resolved": { "intent": "usage", "model": "harrier",
                "legs": [{ "label": "usage", "instruction": "Given an API, … of a Rust codebase, …" }] },
  "warnings": [] }
```

A **workflow intent** (`review`, `refactor`, `doc-sync`, `divergence`,
`onboard`, `security`) answers one list per leg, and `hits` repeats the first
leg for a client that predates `legs`:

```jsonc
{ "hits": [ …legs[0].hits… ],
  "legs": [ { "label": "docs", "instruction": "Given …", "hits": [ … ] },
            { "label": "code", "instruction": "Given …", "hits": [ … ] } ],
  "pairs": [ { "left": { "leg": 0, "hit": 0 },
               "matches": [ { "right": { "leg": 1, "hit": 2 }, "similarity": 0.71 } ] },
             { "left": { "leg": 0, "hit": 1 }, "matches": [ … ], "drifted": true } ],
  "duplicates": [ { "a": { "leg": 1, "hit": 0 }, "b": { "leg": 2, "hit": 3 }, "similarity": 0.91 } ],
  "resolved": { "intent": "doc-sync", "legs": [ { "label": "docs", … }, { "label": "code", … } ] },
  "warnings": [] }
```

- `pairs` (`doc-sync`, `divergence`) match each hit of the first leg with its
  nearest hits of the second, by the cosine of their stored vectors; `drifted`
  flags a best match below `search.drift_below`.
- `duplicates` (`divergence`) are code hits of different files within
  `[search.duplicate_min, search.duplicate_max]`.

Both point into `legs` by position; `legs`, `pairs`, `duplicates` and
`resolved` are absent from the wire when empty. `resolved.model` is the model
profile the search ran under (absent from an older daemon).

In `hybrid` mode (the default), a vector list that cannot be had — no vectors
under the project's model yet, its runtime down, a file holding another model's
vectors — does not fail the search: the keyword hits come back with one
warning naming the model and the repair (`keyword results only: … run
`findery sync -p axle` …`). `"mode": "vector"` returns the error instead
(`409 vectors_unavailable`, `502 upstream`, `409 recipe_mismatch`), and so does
`like`.

A daemon older than `warnings` answered the bare array; `findery-client`
decodes every shape (`SearchWire`), so a newer CLI keeps working against it.

`host_path` is absolute **on the host side**: a hit that cites a container
path is a hit nobody can open.

**Errors**

| Case | Answer |
|---|---|
| an unknown `scope` name | a 400 that lists the ones that exist |
| an unknown `task` value | a 422 |
| an unknown `intent`, or two different intents named through `intent` and `task` | a 400 that lists the known names |
| a `like` locator no chunk covers | a 404 |
| `like` with an intent with no `similar` leg | a 400 |
| a misspelled key (`instructions`) | a 422, not a search that silently ignored it |

### `GET /v1/intents`

The intent library, in library order.

**Request** — `?project=<id>` lists that project's library: the daemon's with
the project's manifest layer on top.

**Response** — `[{ "name", "aliases", "kind": "locate" | "workflow", "summary", "legs": ["callers", …] }]`.

**Errors** — an unknown project is a 404.

### `GET /v1/projects/{id}/files[?glob=<glob>]`

The manifest a pushing client diffs against.

Without it a client would have to send the whole project on every sync.

**Request** — `?glob=crates/**/*.rs` narrows it to the files a path glob names
— the same matcher as a search's `filter.paths`
(`CompiledFilter::matches_file`).

**Response** — every known file with its `content_hash`, `size`, `origin` and
chunk count.

**Errors** — a broken glob is `400 invalid`, a misspelled parameter
(`?globb=`) a 400 too.

### `GET /v1/projects/{id}/chunks?locator=…|id=…[&context=N]`

What a search pointed at.

**Request**

- `locator` — the lines it names (`src/net.rs:L12-L48`, `src/net.rs:12-48`,
  `src/net.rs:L12`)
- `id` — one chunk
- `context` — lines on each side (capped by `search.max_read_context`, 200)

At most `search.max_read_lines` (1000) lines in all.

**Response**

```jsonc
{ "project": "axle", "path": "src/net.rs",
  "host_path": "C:/Users/users/RustroverProjects/axle/src/net.rs",
  "lang": "rust", "start_line": 3, "end_line": 5, "total_lines": 12,
  "text": "/// Retries …\npub fn retry_with_backoff(…) -> bool {\n    attempts < 5",
  "source": "disk",                        // or "index"
  "chunk": { "id": "…", "kind": "function", "symbol": "retry_with_backoff",
             "start_line": 3, "end_line": 6 },
  "warnings": [] }
```

`source: "disk"` is a file the daemon walked, read as it is now (a warning says
when it changed since it was indexed); `"index"` is a pushed or inline document
— or a file the disk can no longer give — rebuilt from its stored chunks, with
a line no chunk holds left empty and reported. A file is read from disk only
when the bucket lists it.

**Errors**

| Case | Answer |
|---|---|
| neither or both of `locator` / `id`, or a locator passed as `id` | `400 invalid` |
| a path with a `..` segment | `400 invalid` |
| a line past the end of the file | `400 invalid`, naming its length |
| a file no chunk of the project covers, an unknown chunk id | `404 not_found` |

### `POST /v1/projects/{id}/documents/batch`

Ingests documents the client walked and read itself, in one round trip.

**Request**

```jsonc
{ "documents": [ { "path": "src/lib.rs", "text": "…" }, … ] }
```

- Files arrive as `origin: "pushed"`.
- Re-sending identical content is free — the hash is compared before anything
  is chunked.

**Response** — a `SyncReport`.

### `POST /v1/projects/{id}/prune`

Pushed files outside the `keep` list are deleted.

**Request** — `keep` is every path the client still has.

```jsonc
{ "keep": ["src/lib.rs", "README.md", …] }
```

> **Note** — **only `origin: "pushed"` rows are touched.** A client can never
> delete what the daemon walked, nor a hand-added inline document. Each side
> prunes what it fed — otherwise one client's sync would silently erase
> another feeder's files.

### `GET /v1/jobs` · `GET /v1/jobs/{id}`

Jobs, with `progress` while running.

**Response** — a **running** job carries `progress`, filled at the moment it
is read:

```jsonc
{ "id": "a3f1…", "project": "axle", "kind": "incremental", "state": "running",
  "progress": { "phase": "indexing",
                "files_seen": 7200, "files_total": 7200,
                "files_unchanged": 6888, "files_pending": 312, "files_done": 96,
                "chunks_done": 1180, "chunks_pending_embed": 2300,
                "embeddings_computed": 1541, "embeddings_cached": 5937,
                "rate": 38.2, "eta_ms": 19900, "elapsed_ms": 42100 } }
```

| Field | Means |
|---|---|
| `phase` | `scanning` (the walk), `indexing`, `pruning` |
| `files_seen` | files the walk accepted so far — what moves while `scanning` |
| `files_total` | every file the walk accepted; `0` while scanning |
| `files_unchanged` | cleared by gate 1 (size and mtime) without being read |
| `files_pending` | `files_total − files_unchanged`: the files this pass reads |
| `files_done` | pending files whose outcome is decided; ends equal to `files_pending` |
| `chunks_pending_embed` | cache misses found so far: what this pass embeds |
| `embeddings_computed` / `embeddings_cached` | moves with every runtime batch / every flush |
| `rate`, `eta_ms` | chunks per second, smoothed; time left for the misses found so far. `eta_ms` is absent without a rate |
| `elapsed_ms` | since the pass started, as of the moment the job is read |

The denominators come from the plan — the walk, then gate 1 over the whole list
— which is published before the first file is read, so the proportion
`files_done / files_pending` is a real one and not a guess.

Every field defaults when absent, in both directions between an older and a
newer daemon. `progress` itself is absent on a queued job (it has not started)
and on a terminal one (its `report` is the description), which is why a client
must treat it as optional.

A **failed** job carries its error as the error body does — `error` is the
payload, `error_code` the `code` from the table above:

```jsonc
{ "id": "a3f1…", "project": "axle", "kind": "full", "state": "failed",
  "error_code": "recipe_mismatch",
  "error": "bucket `work` holds vectors built with `qwen3.7-text-embedding:1024`, but this daemon serves …" }
```

`Job::failure()` turns the pair back into the variant, so a client reports the
failure the daemon raised. `error_code` is absent from a daemon that predates
it; `failure()` then falls back to `internal` with the message intact.

---

## Limits

- `max_body_bytes` (32 MiB) caps request bodies.
- An unknown route returns 404, not 405, so a typo doesn't look like a
  forbidden method.

---

## Verifying

`crates/server/tests/api.rs` exercises the full stack — router, engine,
chunkers, bucket, store — with only the embedder swapped for the mock,
because a model is the one thing a test can't host. Among its cases: the
proof that the second sync computes zero embeddings, and the proof that a
failing background job keeps its `error_code` all the way to `Job::failure()`.
