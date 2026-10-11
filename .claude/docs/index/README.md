# `findery-index` — the engine

Sync, search, jobs, watch. The one place where the bucket, the vector
store, the embedder, and the chunkers meet. Everything above it (HTTP, MCP,
CLI) is a thin translation of `Indexer`.

---

## 1. Incremental sync

### The three gates

```
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

### Writing

In batches (96 files or 512 chunks, whichever comes first) to bound memory
on a first indexing run. For each file:

1. `replace_file_chunks` returns the **vanished** ids
2. those ids are evicted from the vector store
3. new ones are upserted (idempotent on id, CONV-028)
4. the `FileRecord` is rewritten

Intended order: the bucket decides what disappears, the store follows. A
bucket row with no vector self-heals; an orphan vector doesn't.

### Deletions

A known path the walk didn't see again is deleted — **except**
`Origin::Inline` (a document pushed through the API has no file, it must
not vanish on the first scan) and **except** in `Paths` mode (the caller
talked about specific paths, it said nothing about the rest).

### What a `SyncReport` tells you

Every field is a measured fact. `embeddings_computed` against
`embeddings_cached` is the metric that says whether the cache is working.
`warnings` carries non-fatal failures — an unreadable file, non-UTF-8,
over `max_file_bytes` — never a silent `continue` (CONV-022).

---

## 1bis. The same index, fed from two sides

A project is fed either by the daemon walking it, or by a client walking it and
pushing the contents. The walk is the same code (`findery-walk`, shared by both),
and so is the write path (`sync::ingest`) — only who reads the bytes differs.

What changes for the client is which gates it can evaluate. It has no bucket
row to compare `(size, mtime)` against, so gate 1 is unavailable; it reads and
hashes locally, which is cheap, and gate 2 does the filtering. Gate 3, the vector
cache, is server-side and identical in both modes.

`Origin` records who fed each file, and that is what keeps the two sides from
deleting each other:

| Origin | Fed by | Pruned by |
|--------|--------|-----------|
| `file` | the daemon's walk | a daemon-side sync |
| `pushed` | a client's walk | that client's `prune` call |
| `inline` | a hand-written API call | nothing automatic |

## 2. Hybrid search

### What a request means before it is run

A search arrives with a filter, an optional `scope` **name**, and an optional
instruction, over a project that has its own defaults for all three. The
ladder is resolved in one call — `SearchRequest::resolve` — in L0, so the
daemon, the MCP bridge and the CLI cannot answer different questions:

```
request.instruction  >  project.instruction  >  the daemon's configured default
request's filter dimensions  >  the named scope's dimensions
```

The scope is resolved **here**, in the engine, and not by the client that named
it: the MCP bridge is a separate process that never reads a manifest, and the
daemon is the one component both transports already reach. An unknown name is
refused with the list of the ones that exist.

`SearchFilter::layer` is the per-dimension merge. A dimension the request names
replaces the scope's; a dimension it leaves empty keeps the scope's. The
alternative — intersecting them — turns `--scope api --lang markdown` into a
search that matches nothing, and a filter nobody can widen is a filter nobody
can escape. A scope therefore **restricts and never widens**, which is the one
property worth stating: it is a filter applied before scoring, never a second
index and never a cut applied after the ranking.

### Two legs

| Leg | Finds | Misses |
|-------|--------|------|
| vector | "where is retry handled" in a file that never says "retry" | an exact identifier, drowned among its neighbors |
| BM25 | `E0432`, `try_from_utf8_lossy`, a crate name | rephrasing, synonyms |

### The fusion

Reciprocal Rank Fusion on **rank**, not on score:

```
score(d) = Σ  1 / (rrf_k + rang_i(d) + 1)
```

Cosine lives in `[-1,1]`, BM25 is unbounded. Calibrating one against the
other would be a permanent, wrong tuning job; fusing by rank needs no
calibration at all. `rrf_k = 60` comes from the original paper and holds up
well here.

Ties are broken **by id**: two identical queries must return the same
list, otherwise any cache sitting above this one lies.

In single-leg mode, the native score is kept: an operator reading `0.83` on
a vector search should see a cosine, not an RRF artifact that means nothing
in isolation.

### Filters

`CompiledFilter` is the SSOT (CONV-011). The vector leg filters store-side;
the BM25 leg has no payload, so it over-fetches and is narrowed down once
the chunks are in hand. A malformed glob is **rejected** rather than
silently matching nothing.

### Token budget

`max_snippet_chars` truncates each excerpt **on a line boundary** and marks
it `… [truncated]` — cutting mid-token while implying the file stops there
would be a lie. It's the most direct lever an MCP client has over its own
spend.

---

## 3. Jobs

A first indexing run takes minutes. An MCP client that blocks on that HTTP
call times out, retries, and kicks off a **second** sync. Hence jobs: the
request returns an id right away, and a semaphore bounds concurrency so two
clients can't saturate the embeddings runtime between them.

`has_active()` lets the server refuse to stack a redundant sync: it returns
the job already running.

History bounded in memory (`job_history`). It's not persisted: a job is
session-scoped information, and the `SyncReport` that counts is the one
that's already been returned.

### Progress, without a callback climbing a layer

A job that runs for minutes and says nothing is indistinguishable from one that
has hung, so a running job carries `progress` — and the way it gets there is the
part worth knowing.

A callback would be an edge climbing back up the layers (CONV-005), so there is
none. `JobRegistry` hands the sync a `ProgressHandle` it writes into, and reads
the same handle back when a client asks. The value goes **down** as an argument
and comes back **up** as a return value: no crate edge is added, and the handle
holds no `Indexer` and no store, so it cannot grow into a second engine.

| Emission | When |
|-----------|------|
| the walk returns | `phase = indexing`, `files_total` — the denominator, known before the first byte is read |
| every 512 files | `files_done`, so an incremental pass that flushes nothing still advances |
| every flush | `chunks_done`, `embeddings_computed`, `embeddings_cached` |
| the delete pass | `phase = pruning` |

A **terminal** job carries no progress at all: its numbers are its `SyncReport`,
and two records of one fact are free to disagree. So a `Failed` job reports no
counters either — "50 of 200" on a job that stopped is progress that is not
coming.

Callers nobody watches — `findery watch`, the inline and pushed write paths —
pass `ProgressHandle::detached()`, which costs one uncontended lock. The engine
is written once rather than forked into a watched and an unwatched copy.

---

## 4. Watch

`notify` in recursive mode, **content** events only — an access or
metadata event fires constantly on an active checkout and would keep the
debounce window open forever.

Touched paths are coalesced by path, then sent as a single
`SyncMode::Paths` after a `debounce` quiet period (1500 ms by default). An
editor that writes in bursts — save, formatter, build script — would, if we
reacted per event, re-embed the same file three times a second.

`WatchHandle` stops watching when it's dropped: the server keeps a map per
project, and replacing an entry is enough to kill the previous one.

---

## 5. Path safety

`sanitize_rel` normalizes separators, strips the leading `/`, and **rejects
any `..` segment**. This string arrives from an MCP tool call:
`../../etc/passwd` has to die right there, before it becomes a
`root.join(..)`.

`PathMapper` then translates host ↔ container, at a segment boundary — a
raw `starts_with` would make `/workspaces-old` match against `/workspaces`.
