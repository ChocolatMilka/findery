# `findery-index` — the engine

Sync, search, jobs, watch, and the lifecycle of projects and buckets. The one
place where the buckets, the vector stores, the embedders and the chunkers
meet. Everything above it (HTTP, MCP, CLI) is a thin translation of `Indexer`
— jobs and watchers included, so a handler is validate → one call → serialize
(CONV-002).

---

## 0. Which file an operation works in

### Families and members

A **family** (`storage/families.rs`, `BucketFamily`) is a bucket as everything else
names it: a name and a base path. Its **members** are its files, one per
recipe (`bucket.<slug>-<tag>.db`, `.claude/docs/store/README.md` §0), keyed by
`RecipeTag`.

A `BucketHandle` is one open member:

- **`tag`** — what the resolver finds it under
- **`recipe`** — what that tag is expected to hold
- **`stored_recipe`** — what the file records
- its store

`stored_recipe` is the anti-collision guard: `ensure_serves(recipe)` refuses
with `RecipeMismatch` unless the file holds exactly the recipe asked for — a
file renamed by hand, or a tag collision, is never trusted.

`BucketFamily::info` (what `GET /v1/buckets` lists) reports every open member
through `BucketHandle::info`: path, tag, recipe, `legacy`, `serving` (the
member of the default model's recipe), its own projects, chunks and size, and
its `recipe_conflict()`.

### Leases and gates (`gates.rs`)

Every operation on a project runs under a **`Lease`**, taken through
`Indexer::lease(id, Access::{Read, Write})`:

1. Wait at the project's **gate** (the read side of a tokio `RwLock`; a drain
   holds the write side, §5). The gate records which task holds each lease, so
   a drain can tell a job holding the project from a search beside it.
2. Read the project's freshest row across the family's members.
3. Pick its model — the spec's pin, else the pool's default — and that model's
   recipe (from the built embedder, or the width the profile knows without
   building — `.claude/docs/embed/README.md`, the pool).
4. Find the member of that recipe: **a write creates it**, under the family's
   `create` mutex; **a read never creates a file**, and neither does a family
   once closed (`BucketFamily::close` marks it; a creation after it is
   `NotFound`, never a file nobody would close).

A write's recipe comes from `EmbedderPool::write_recipe`: the built embedder's
width, else the width known without it, else the build's own failure. A lease
whose embedder turns out to serve another recipe than the one it resolved is
refused with `RecipeMismatch` (`Error::recipe_drift`).

The lease carries:

- the project row (with `Project.model` stamped)
- the alias
- the recipe
- the member (always present for a write)
- the family
- an `embedder()` accessor that builds a lazy profile on first use and refuses
  an embedder that turned out to serve another recipe than the lease resolved

A new member is **seeded with the project's row only**, copied from the
freshest sibling. `files` and `chunks` are not copied: gate 1 would then skip
every file on the next sync and leave chunks without a vector under the new
model.

With the row alone the next sync re-chunks (pure, cheap) and embeds exactly
what the new model has never seen — and a model switched back to costs
nothing, its member never moved.

| Operation | Access | Member it touches |
|---|---|---|
| `sync`, `add_document`, `push_documents` | write | the model's, created if missing |
| `register` | **write side of the gate** (`hold_still`) | the model's, created if missing — every member gets the row |
| `search`, `stats`, `export`, `file_manifest` | read | the model's when it exists (see §2 for search) |
| `prune`, `remove_document` | read | **every** member: a document the user deleted must not come back the day they switch model |
| `get_project`, `list_projects` | none | the freshest row; no gate, no embedder — a lookup answers while a sync holds the project and its runtime is down |

### Registration

`register` writes the project row into **every** member of its family, so
scopes, excludes and the pin never diverge between models.

It holds the project still for the whole registration — the write side of the
gate, waited for like a drain waits (every lease let go) but asking nothing to
stop, within `server.drain_timeout_secs`, else `busy` naming the job. A
registration that moves the project therefore never runs beside a sync writing
it.

The move writes the target first (member, row everywhere, store), then the
home, and leaves the old bucket **last**: a registration that fails on the way
has moved nothing, and an old bucket that cannot be emptied is a warning — the
copy left behind is reported at the next boot as a duplicate row.

A row that cannot be read (a closed member, a SQLite failure) is an error,
never "not attached".

A registration of a project that exists is a **merge** (`merged_spec`): each
field the request leaves absent keeps the project's current value — its
bucket, scopes, instruction, pin, include/exclude and extension lists,
walk flags — and only an explicitly empty value clears one (`[]`, `{}`, `""`
for `instruction` and `model`).

A request with no `id` reuses the id of the project already attached at that
root, so registering a root again is never a second project. On Windows a root
matches another without case; elsewhere `App` and `app` are two projects.

The previous row is the attached one, else the detached row in the target
family — and with no bucket in the request, a detached project goes back to the
family `detach` recorded (`detached[id]`), so the MCP bridge, which registers
with a root alone, reattaches the index instead of starting a new one.

INTENT: the MCP bridge and scripts register with a root alone; before this,
every such call wiped what the manifest had declared.

`findery init`, which owns the manifest, sends each field it declares — empty
ones included — so a removal in the manifest reaches the daemon.

### Homes

Which family an attached project lives in is rebuilt at boot from the rows on
disk. A project id found in two families lives in the one holding the
**freshest** row (`updated_at`, then boot order — default first, then names),
and the duplicate is reported in `health.notices`.

The old map took whichever family it met last, silently.

### Boot (`Indexer::new`)

The order is the point, each step a precondition of the next:

1. The vocabulary migration (`catalog.db` → `bucket.db`).
2. `buckets.toml`.
3. The removals a previous run left unfinished (§5) — before anything opens a
   file a removal still has to delete. One that still cannot finish is a
   notice, and its family (or member) is not opened. A tombstone for
   `default` — only a hand edit writes one — is refused and dropped.
4. Legacy adoption (`adopt_all`, on the blocking pool) — before anything opens
   a handle that would hold the file, and skipping the families step 7 leaves
   closed.
5. A scan of every family, peeking at the recipe each member records without
   opening it.
6. The embedder pool — the default profile built now, at the width its
   buckets remember when its runtime is down.
7. The members opened, keyed by tag:
   - A legacy file kept in place serves its recipe (a tagged member of the
     same recipe wins over it).
   - A base file with no recipe is opened as a **source of rows** — its
     projects get a home, and `freshest_row`, `project_ids` and
     `put_row_everywhere` read it — and the first write claims it instead of
     shadowing it with a new file.
   - A family whose projects are **all** detached is not opened.
   - A row that cannot be read is in `health`, never a project silently gone.

---

## 1. Incremental sync

### The three gates

```text
candidate from the walk
   │
   ├─ gate 1: (size, mtime) unchanged? ─────▶ unchanged      (nothing is read)
   │
   ├─ read + blake3
   │
   ├─ gate 2: content_hash unchanged? ──────▶ unchanged      (nothing is chunked)
   │           (mtime is refreshed so gate 1 bites next time)
   │
   ├─ chunk → materialize (id, ordinal, content_hash)
   │
   └─ gate 3: (recipe, content_hash) cached? ──▶ vector reused
               otherwise ──▶ embedding
```

`--full` short-circuits gates 1 and 2, **never gate 3**: rebuilding an
index on an unchanged repo costs no call to the model.

### Planned, then worked by two overlapping halves

A pass is **planned** before anything is read (`sync/plan.rs`): the walk, then
gate 1 over the whole list — a map lookup per file, microseconds for the tree.
How many files are left to read is therefore known before the first one is
read. `--full` clears nothing at gate 1, so every walked file is pending.

The pending files are then **worked** by two futures of the same task, joined
(`sync/pipeline.rs`):

| Half | Does | Owns |
|---|---|---|
| scanner | read → blake3 → gate 2 → chunk, one outcome per file | nothing it writes |
| writer | the gate-2 metadata refresh, the batches, the flushes, the warnings | the member and the `SyncReport` — the single writer (CONV-024) |

A bounded channel (two flushes' worth of files) lets the scanner run ahead
while a batch embeds, and no further.

Because both halves are joined in one task, no task outlives the pass: a drain
stops the scanner at its next file and the writer at its next flush — **the
flush in flight finishes**, so the vectors it paid for are written. A failed
writer drops the channel and the scanner stops at its next send.

An unreadable or non-UTF-8 file is an outcome like any other: a warning in the
report, never a failed pass (CONV-022).

Measured against a runtime of fixed speed (one batch at a time, so concurrency
cannot help — only the overlap can), 1 500 generated files, 9 000 chunks,
release build, median of three:

| Runtime cost per chunk      | Before the overlap | After  |       |
|-----------------------------|--------------------|--------|-------|
| 400 µs (3.6 s of embedding) | 5.22 s             | 4.94 s | −5 %  |
| 100 µs (0.9 s)              | 2.57 s             | 2.14 s | −17 % |
| 0 (no runtime cost)         | 1.78 s             | 1.32 s | −26 % |

What remains above the runtime's own time (≈ 1.3 s) is the writer's SQLite
work, which stays serial with the embedding by design: it is the single writer.
Reproduce with `cargo test -p findery-index --release -- --ignored --nocapture
a_first_sync_runs_at` (`FINDERY_BENCH_FILES`, `FINDERY_BENCH_US_PER_CHUNK`).

### Writing

In batches (`sync.flush_files` 96 or `sync.flush_chunks` 512, whichever
comes first) to bound memory on a first indexing run; the scanner reads at most
`sync.scan_ahead_flushes` (2) flushes ahead.

A flush probes the cache once for the batch, then embeds the misses in slices
of the profile's `batch_size`, `concurrency` at a time, in order — each slice
is one runtime call, and is counted the moment it lands.

The `Embedder` trait is unchanged (CONV-004); the embedder's own semaphore
still bounds what reaches the runtime.

For each file:

1. `replace_file_chunks` returns the **vanished** ids
2. those ids are evicted from the vector store
3. new ones are upserted (idempotent on id, CONV-028)
4. the `FileRecord` is rewritten

Intended order: the bucket decides what disappears, the store follows. A
bucket row with no vector self-heals; an orphan vector doesn't.

### Deletions

A known path the walk didn't see again is deleted — **except**:

- **`Origin::Inline`** — a document pushed through the API has no file, it
  must not vanish on the first scan
- **in `Paths` mode** — the caller talked about specific paths, it said
  nothing about the rest

### What a `SyncReport` tells you

Every field is a measured fact. `embeddings_computed` against
`embeddings_cached` is the metric that says whether the cache is working.

`warnings` carries non-fatal failures — an unreadable file, non-UTF-8, over
`max_file_bytes`, a targeted path the walk rules refuse — never a silent
`continue` (CONV-022).

A vector store that cannot count fails the pass rather than guessing.

### A targeted sync is the same walk, narrowed

`SyncMode::Paths` (a watcher's touched paths, `add_document` without text, a
`sync {paths}`) runs `findery_walk::walk_paths`: the project's walk with only
the directories on the way to each requested path listed.

Each path is sanitized (`sanitize_rel`), then admitted only when the full walk
would admit it — `.gitignore`, hidden entries, include/exclude, extensions, the
size ceiling, and the pruned `.git` / `.findery` directories. A refusal is a
warning naming the reason.

The rules that need no disk are `findery_walk::Rules`, the one matcher behind
the walk, the targeted sync and the watcher.

---

## 1bis. The same index, fed from two sides

A project is fed either by the daemon walking it, or by a client walking it and
pushing the contents.

The walk is the same code (`findery-walk`, shared by both), and so is the write
path (`sync::ingest`) — only who reads the bytes differs.

What changes for the client is which gates it can evaluate:

- **Gate 1** — unavailable: it has no bucket row to compare `(size, mtime)`
  against
- **Gate 2** — does the filtering: it reads and hashes locally, which is cheap
- **Gate 3** — the vector cache, server-side and identical in both modes

`Origin` records who fed each file, and that is what keeps the two sides from
deleting each other:

| Origin   | Fed by                  | Pruned by                  |
|----------|-------------------------|----------------------------|
| `file`   | the daemon's walk       | a daemon-side sync         |
| `pushed` | a client's walk         | that client's `prune` call |
| `inline` | a hand-written API call | nothing automatic          |

---

## 2. Search (`search/`)

### What a request means before it is run

A search arrives with a filter, an optional `scope` **name**, an optional task
or instruction, over a project that has its own defaults.

The ladder is resolved in one call — `SearchRequest::resolve` — in L0, so the
daemon, the MCP bridge and the CLI cannot answer different questions (the
ladder itself: `embed/README.md`). Its inputs are read here, in the engine:
the project's scope of that name (an unknown one is refused with the list),
and the spec's own `instruction` / `task`.

The scope is resolved **here** and not by the client that named it: the
daemon holds the project's definition, and a client reads a manifest only to
learn which project it stands in — never to expand a scope.

`SearchFilter::layer` is the per-dimension merge: a dimension the request names
replaces the scope's, one it leaves empty keeps the scope's. Intersecting them
instead would turn `--scope api --lang markdown` into a search that matches
nothing.

A scope **restricts and never widens**: a filter applied before scoring, never
a second index and never a cut applied after the ranking.

### Running it

```text
resolve → member with the model's vectors
        → the query embedded once
        → the query's BM25 list
        → vector list + BM25 list → fused → hydrated
```

Both lists are read from ONE member: a chunk id means nothing in another file.

`SearchResponse` is `{ hits, warnings }`: the hits best first, and why the
answer is narrower than asked, each warning naming the repair.

### Two lists — and what happens when one cannot run

A search fuses a **vector list** and a **BM25 list**:

| List   | Finds                                                      | Misses                                           |
|--------|------------------------------------------------------------|--------------------------------------------------|
| vector | "where is retry handled" in a file that never says "retry" | an exact identifier, drowned among its neighbors |
| BM25   | `E0432`, `try_from_utf8_lossy`, a crate name               | rephrasing, synonyms                             |

In **hybrid** mode, a vector list that cannot be had becomes a warning and the
BM25 list answers alone:

| The vector list fails with | Because | The warning says |
|---|---|---|
| `VectorsUnavailable` | the project has no member, or no vector, under its model yet | `findery sync -p <id>` |
| `Upstream` / `Embed` from the embedder | the model's runtime is down, or answered off-contract | start the runtime |
| `Upstream` from another service (`qdrant`) | the vector store is down | start the vector store |
| `RecipeMismatch` | the member file holds another model's vectors, or the runtime now serves another width than remembered | the rename, the rebuild, or a retry |

The BM25 list then reads the model's member when it holds chunks, else the
sibling member with the freshest row that does — BM25 needs no vector, so any
model's chunks answer it, which keeps a search useful right after a model
switch.

A search in **vector** mode makes the same failure the answer (409 for
`VectorsUnavailable`).

The candidate count is `k × search.candidate_multiplier`, capped at
`search.max_k × search.candidate_ceiling_multiplier`; a BM25 list narrowed
after the fact over-fetches `search.keyword_overfetch` times.

### The fusion

Reciprocal Rank Fusion on **rank**, not on score:

```text
score(d) = Σ  1 / (rrf_k + rank_i(d) + 1)
```

Cosine lives in `[-1,1]`, BM25 is unbounded. Calibrating one against the
other would be a permanent, wrong tuning job; fusing by rank needs no
calibration at all.

`rrf_k = 60` comes from the original paper and holds up well here.

Ties are broken **by id**: two identical queries must return the same
list, otherwise any cache sitting above this one lies.

With one list only (a vector or keyword search), the native score is kept: an
operator reading `0.83` on a vector search should see a cosine, not an RRF
artifact that means nothing in isolation.

### Filters

`CompiledFilter` is the SSOT (CONV-011). The vector list filters store-side;
the BM25 list has no payload, so it over-fetches and is narrowed down once
the chunks are in hand. A malformed glob is **rejected** rather than
silently matching nothing.

### Token budget

`max_snippet_chars` truncates each excerpt **on a line boundary** and marks
it `… [truncated]` — cutting mid-token while implying the file stops there
would be a lie.

It's the most direct lever an MCP client has over its own spend.

---

## 3. Jobs

A first indexing run takes minutes. An MCP client that blocks on that HTTP
call times out, retries, and kicks off a **second** sync.

Hence jobs: the request returns an id right away, and a semaphore bounds
concurrency so two clients can't saturate the embeddings runtime between them.

### Admission

Jobs are spawned **by the engine**: `Indexer::start_sync(id, &SyncRequest)`
admits every request through `JobRegistry::active_or_create`, under one lock:

- a request for the **same pass** (same mode; for `paths`, the same set) joins
  the job already queued or running
- any other pass is a new job, **queued behind** the running one — two passes
  of one project never write side by side, since a sync takes the project's
  turn (`ProjectGate::take_turn`) before its lease

The job's task is spawned and its `AbortHandle` tracked beside its project. A
handler that spawned the task itself — as it once did — left no engine method
able to stop it, which is what `detach` needs (§5).

When the request answers:

- **with `wait`** — the request answers once its job ended (report, or the
  job's error)
- **with `wait_ms`** — it waits at most that long, capped by
  `server.max_sync_wait_ms` (120 s), and answers the report when it ended in
  time, the job's own error when it failed, and the job as it stands (live
  progress included) otherwise

### How a job ends

A job records its end however its task ends: the task owns a guard (`JobEnd`)
whose `drop` fails the job — `cancelled` for an abort, `internal` for a panic —
and untracks it.

Only the **first** end of a job counts (`succeed`/`fail` act on a job that has
not ended), so a late abort never overwrites a success. Before the guard, an
aborted task stayed `running` forever: answering every later sync, holding its
watcher back.

`JobRegistry::wait` is woken by the `Notify` every `succeed`/`fail` fires,
registered before the state is read, so an end that lands in between is never
missed and nothing polls.

History bounded in memory (`job_history`). Only **finished** jobs are evicted,
oldest first: a queued or running job stays findable — a retry must join it, a
drain find it.

It's not persisted: a job is session-scoped information, and the `SyncReport`
that counts is the one that's already been returned.

A failed job keeps its error the way the wire keeps one: `JobRegistry::fail`
takes the `Error` itself and stores `error_code` (`Error::code()`) beside
`error` (`Error::detail()`, the payload without the variant's prefix).

`Job::failure()` rebuilds the variant through `Error::from_wire`, and the
client, the CLI and the MCP bridge all read a failure through it — so a sync
refused for a recipe mismatch ends as `findery: recipe mismatch: …` with its
repair, not as `internal: config: …`, which blamed a bug that was not there.

### Progress, without a callback climbing a layer

A job that runs for minutes and says nothing is indistinguishable from one that
has hung, so a running job carries `progress` — and the way it gets there is the
part worth knowing.

A callback would be an edge climbing back up the layers (CONV-005), so there is
none. `JobRegistry` hands the sync a `ProgressHandle` it writes into, and reads
the same handle back when a client asks.

The value goes **down** as an argument and comes back **up** as a return value:
no crate edge is added, and the handle holds no `Indexer` and no store, so it
cannot grow into a second engine.

| Emission | When |
|---|---|
| every file the walk accepts | `phase = scanning`, `files_seen` |
| the plan (gate 1 over the list) | `phase = indexing` with `files_total`, `files_unchanged` (cleared by gate 1, never read) and `files_pending` — in one write, before the first read |
| every pending file whose outcome is decided | `files_done` += 1 — a gate-2 hit, a warning; a batch's files when its flush is written. It ends **equal** to `files_pending`, never past it |
| every flush's cache probe | `embeddings_cached`, `chunks_pending_embed` (the misses found so far) |
| every embedding slice that lands | `embeddings_computed`, `rate`, `eta_ms` |
| every file written | `chunks_done` |
| the delete pass | `phase = pruning` |

`rate` (chunks/s) and `eta_ms` are computed in L0 by `Progress::observe`,
purely: a moving average weighted by **time**, not by sample, so two slices
landing a millisecond apart do not read as a burst.

Its clock starts at the first miss, so the walk does not count against it. The
ETA covers the misses found so far — a pass with files still to scan can find
more — and is absent without a rate or without misses to count down from.

The handle owns the clock: `begin()` starts it when the pass starts (a queued
job has none), and every write and every snapshot stamps `elapsed_ms`, so a
client reads the time now, not the time of the last write.

A **terminal** job carries no progress at all: its numbers are its `SyncReport`,
and two records of one fact are free to disagree.

So a `Failed` job reports no counters either — "50 of 200" on a job that stopped
is progress that is not coming.

Callers nobody watches — `findery watch`, the inline and pushed write paths —
pass `ProgressHandle::detached()`, which costs one uncontended lock. The engine
is written once rather than forked into a watched and an unwatched copy.

---

## 4. Watch

`notify` in recursive mode, **content** events only — an access or
metadata event fires constantly on an active checkout and would keep the
debounce window open forever.

Touched paths are coalesced by path, then sent as a single
`SyncMode::Paths` after a `debounce` quiet period (1500 ms by default).

An editor that writes in bursts — save, formatter, build script — would, if we
reacted per event, re-embed the same file three times a second.

A touched path the project's walk rules refuse (`findery_walk::Rules`:
`.findery/`, `.git/`, an exclude, an extension the project does not walk) is
dropped on arrival: a bucket inside the tree writes on every sync, and syncing
its own writes would never end.

The watchers belong to the engine (`Indexer::set_watch`, one per project;
replacing one drops the previous handle, which stops its task).

A watcher's sync is a **job** like any other: while one is already running on
the project, the touched paths wait for the next quiet period — kept, not
dropped — instead of racing it over the same files. A batch whose job failed is
given back and carried by the next sync. A watcher whose project is gone
(`NotFound`) exits.

The task holds the engine weakly: the engine owns the handle that stops it, and
a strong reference back would keep both alive forever.

---

## 5. Detach, rm, bucket rm (`lifecycle.rs`)

### The drain

Each verb ends with a file the user wants to do something with, so each first
**drains** the project:

1. It adds a cancel request to the gate — a counter, so one drain giving up
   never withdraws another's. A sync checks it between files and before every
   flush, and stops with `Cancelled`: the job ends `failed` with code
   `cancelled`.
2. It then takes the write side of the gate, which waits for every lease in
   flight and keeps new ones out.
3. Past `server.drain_timeout_secs` (15 s) it aborts the project's running job
   **only when that job is the only holder of the gate**, records it
   `cancelled` at once, and waits `server.abort_grace_secs` (2 s) more.

Anything else holding the gate — a search, an inline lease — is not the drain's
to abort, and aborting the job beside it would free nothing: the drain gives up
with `Busy` naming the job — **having changed nothing**: the gate reopens and
its cancel request is withdrawn.

Committed, the gate stays shut (waiting leases get `NotFound`) and the watcher
is stopped; a sync it queued meanwhile meets the shut gate and ends
`not_found`. A second drain that waited behind a committed one gets `NotFound`.

### The verbs

| Verb | Order |
|---|---|
| `detach(id)` | drain → `buckets.toml` `detached[id]` → forget the home |
| `forget_project(id)` (`rm`) | drain → **tombstone** (`detached[id].removing`) → vectors and rows out of every file → files → registry entry **last** |
| `forget_bucket(name, None)` | drain every attached project → **tombstone** (`removing_families`) → vectors → close → files → registry **last** |
| `forget_bucket(name, Some(model))` | drain the projects whose model writes that member → **tombstone** (`removing_members`) → its vectors → close and delete its file set, under the family's `create` lock → tombstone cleared |
| `register` of a detached project | reopen the family's files |

What each ends with:

- **`detach(id)`** — every member **closed** when no attached project is left
  in the family (and it is not `default`): unlocked, not reopened at boot.
  `ProjectDetached { closed, still_open }`.
- **`forget_project(id)`** — rows only when another project has a row in the
  bucket's **files** or is detached into it, or it is `default` — read from the
  files, never from the map of homes, which misses a row it could not read;
  otherwise the bucket's files (its whole directory at a derived path), legacy
  included, and `.findery/config.toml` stays.
  `ProjectRemoved { files_deleted, rows_only, shared_with }`. A detached
  project is removed the same way.
- **`forget_bucket(name, None)`** — `BucketRemoved`; `default` is refused
  first. A bucket whose projects are all detached is found on disk (registry,
  detached entry, or derived path), opened for the removal only, and removed
  the same way.
- **`forget_bucket(name, Some(model))`** — `model` is an alias, or the member's
  tag or recipe — what still names the file of a profile no longer configured;
  allowed on `default` too; the bucket's last member is refused: that is the
  bucket. The drains are released, not committed — the projects stay, on their
  other members, and a sync under another model never notices.
- **`register` of a detached project** — **instant**: nothing is re-indexed.

### Removals, at boot and online

Boot and online removals share one module (`lifecycle/removal.rs`): the same sharing
rule, the same order — vectors before rows, rows before files — and the same
cost of a failing vector store: it stops the removal and the tombstone stays,
since a collection left behind would be inherited by the next project of the
same id and model.

A file another process holds stops `rm` with `Busy` (`Error::busy_removal`: the
removal stopped part-way, and says so) and keeps the tombstone. The next boot
finishes the removal before anything opens the files, so nothing is orphaned
and nothing comes back.

Registry and file work runs on the blocking pool (`storage/blocking.rs`), never on a
tokio worker.

---

## 6. Path safety

`sanitize_rel` normalizes separators, strips the leading `/`, and **rejects
any `..` segment**. This string arrives from an MCP tool call:
`../../etc/passwd` has to die right there, before it becomes a
`root.join(..)`.

A targeted sync adds a second lock: a sanitized path is admitted only when the
walk rules admit it (§1).

`PathMapper` then translates host ↔ container, at a segment boundary — a
raw `starts_with` would make `/workspaces-old` match against `/workspaces`.
