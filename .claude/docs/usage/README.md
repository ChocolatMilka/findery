# Using it well

The quickstart in the README gets you searching. This page is the rest: every
knob, what it actually changes, and when reaching for it is the right move.

---

## 1. Two ways to run it

Both modes run **the same daemon binary**; only its packaging differs. The
container next to it is never the server — it is a stateless third-party
inference runtime that turns text into vectors and keeps nothing.

The model is heavy and benefits from a container. The daemon is a 5 MB Rust
binary with an embedded SQLite — containerizing it on a dev machine buys nothing
and costs the bind-mount boundary.

### Native daemon — the default

```bash
make install        # findery, finderyd, findery-mcp → ~/.cargo/bin
make up-model       # the model, in Docker
finderyd     # the daemon, on this machine
```

The daemon sees your real filesystem. **Any path on any disk is indexable**, no
mount to declare, no path translation. `FINDERY_WORKSPACES` is irrelevant here.

### All-Docker

```bash
cp .env.example .env    # FINDERY_WORKSPACES = the parent of your repositories
make up
```

The daemon only sees what is bind-mounted. Every project must live under
`FINDERY_WORKSPACES`; `FINDERY_MOUNTS` translates container paths back to host paths so
hits stay openable. Extra roots go in `docker-compose.override.yml` — and a
mount without its matching `FINDERY_MOUNTS` entry is the classic mistake.

Pick this for a shared server or CI, not for a laptop.

### Where things land

| What | Native | All-Docker |
|------|--------|------------|
| Model weights | volume `findery_hf-cache` | same |
| Catalog + vectors | `$FINDERY_DATA_DIR/catalog.db`, default `%LOCALAPPDATA%/findery` (or `~/.local/share/findery`) | volume `findery_findery-data` |
| Exports | `$FINDERY_DATA_DIR/exports` | `/data/exports` in that volume |

`finderyd --check` prints the configuration as actually resolved. One
`catalog.db` holds every project; it is the file worth backing up.

---

## 2. Everyday commands

Every project-scoped command finds the project containing the current
directory. `-p <id>` overrides it, from anywhere.

```bash
cd D:/work/my-project
findery init                       # register + index
findery search "where is retry handled"
findery sync                       # after editing — near-instant
findery status                     # files, chunks, vectors, model, disk
findery projects                   # all of them, current one marked *
```

| Command | What it does |
|---------|--------------|
| `findery init [PATH]` | registers a directory and indexes it. `--id` to name it, `--ext rs,md,toml` to choose extensions, `--no-sync` to register only |
| `findery sync` | incremental. `--full` ignores the metadata and hash gates (never the vector cache). `--path src/a.rs` targets files. `--background` returns a job id |
| `findery watch` | re-syncs on file changes, debounced. `--off` stops it |
| `findery doc add <path> --stdin` | indexes text with no file behind it — notes, decisions, generated docs |
| `findery doc rm <path>` | drops one document |
| `findery export --format binary` | vectors + metadata out of the index |
| `findery rm` | removes a project, its chunks and its vectors |
| `findery health` | what the daemon is actually serving |
| `findery jobs [id]` | background sync jobs |

`--json` on any command prints the daemon's answer verbatim, for scripting.

---

## 3. Searching precisely

### Retrieval intent — the cheapest accuracy you will get

Qwen3-Embedding is instruction-aware: a task-tailored instruction buys 1-5%
retrieval accuracy for the cost of one prefix on a string that was going to be
embedded anyway.

```bash
findery search "QueryTask" --task symbol      # you have a name, you want its definition
findery search "retry_with_backoff" --task usage    # you want the call sites
findery search "why rank fusion" --task docs        # rationale, not implementation
```

| `--task` | Use it when |
|----------|-------------|
| `code` *(default)* | a general "where is X handled" question |
| `symbol` | you have a name and want the declaration |
| `usage` | you have an API and want its callers |
| `docs` | you want design rationale, not code |
| `test` | you want the tests pinning a behaviour |
| `config` | a setting, a flag, an environment variable |
| `error` | a message, a panic, a diagnostic code |

`--instruct "…"` overrides the preset with free-form text. Describe the TASK,
not the query — restating the query duplicates signal the query already carries
and performs worse than the default.

### Mode

```bash
findery search "E0432" --mode keyword     # exact identifier: skip the semantics
findery search "how do we avoid re-embedding" --mode vector
```

`hybrid` (the default) fuses both by rank and beats either one alone on code.
Reach for `keyword` when you know the exact string, `vector` when the wording in
the code is nothing like the wording in your head.

### Filters — applied before scoring, not after

```bash
findery search "cache" --path 'crates/store/**' --lang rust --kind method
```

`--path` takes globs, `--lang` one of `rust markdown toml other`, `--kind` one of
`file function method struct enum trait impl module macro test const doc_section
text`. A malformed glob is rejected, never silently matched against nothing.

### Token budget

The two levers that matter, in order:

```bash
findery search "…" --max-chars 600    # per-excerpt ceiling — the strongest lever
findery search "…" -k 3               # fewer excerpts
```

`k=8` at 1200 characters is roughly 2,500-3,500 tokens for a full answer.
Truncation happens on a line boundary and is marked `… [truncated]`.

---

## 4. Choosing what gets indexed

Per project, at `findery init` or by re-registering:

```bash
findery init --ext rs,md,toml,yml,sql
```

Four independent levers, applied in this order:

| Lever | Default | Effect |
|-------|---------|--------|
| `extensions` | `rs`, `md` | empty means everything that survives the rest |
| `include` | none | globs; narrows to these |
| `exclude` | build output + secrets | globs; subtracts |
| `follow_gitignore` | true | honours `.gitignore` even outside a git repo |
| `include_hidden` | true | descends into `.github/`, `.cargo/`, `.claude/` |

Dot-directories are indexed **by default**, because `.github/workflows`,
`.cargo/config.toml` and design notes under `.claude/` are exactly what people
ask about. The default `exclude` list is what keeps credentials out: `.env*`,
`.ssh/`, `.aws/`, `.gnupg/`, `.netrc`, `.npmrc`, `*.pem`, `*.p12`, `id_rsa*`,
`id_ed25519*`. Drop a pattern if you want that file indexed.

Files that are not valid UTF-8, or above `max_file_bytes` (2 MB), are skipped
and **reported** in `warnings` — never dropped in silence.

**Chunking quality differs by language.** `.rs` gets a syntactic chunker (one
chunk per item, with symbol and breadcrumb), `.md` a heading-based one.
Everything else falls back to line windows: usable, but with no symbol. Adding a
real chunker for a language is one `Chunker` implementation plus one line in
`Registry`.

Global defaults for every project: `FINDERY_EXTENSIONS=rs,md,toml` and
`FINDERY_INCLUDE_HIDDEN=false`.

---

## 5. Choosing the model

Nothing about the model is compiled in. Backend, URL, route, auth header, name
and width are all data.

```bash
FINDERY_EMBED_MODEL=Qwen/Qwen3-Embedding-0.6B   FINDERY_EMBED_DIM=1024   # default, CPU-friendly
FINDERY_EMBED_MODEL=Qwen/Qwen3-Embedding-4B     FINDERY_EMBED_DIM=2560   # GPU
FINDERY_EMBED_BASE_URL=https://api.openai.com   FINDERY_EMBED_API_KEY=sk-…
```

Changing model or dimension opens a **new** cache namespace
(`recipe = "<model>:<dim>"`). The daemon refuses to mix two recipes in one
catalog and says so at startup: keep the old settings, point `FINDERY_DATA_DIR`
elsewhere, or delete the catalog and re-index.

At boot the daemon sends one throwaway input and compares the returned width to
`FINDERY_EMBED_DIM`. A mismatch is fatal — half a corpus at the wrong width is
silently unsearchable, and this is the only cheap moment to catch it.

Throughput: `FINDERY_EMBED_BATCH_SIZE` (inputs per request) and
`FINDERY_EMBED_CONCURRENCY` (batches in flight). Raising them past what the runtime
can absorb only lengthens the queue.

---

## 6. Choosing the vector store

| Backend | Search | Extra service | When |
|---------|--------|---------------|------|
| `sqlite` *(default)* | exact in-RAM scan — 12 ms at 50k chunks, 50 ms at 200k | none | up to ~150k chunks |
| `qdrant` | HNSW, payload filters, int8 quantization | one local container | beyond that, or a shared index |

```bash
FINDERY_STORE_BACKEND=qdrant docker compose --profile qdrant up -d
```

Switching backends costs a re-sync — which re-embeds nothing, because the vector
cache survives. Figures are reproducible with `make bench`; the reasoning,
including why an *embedded* ANN index was measured and rejected, is in
[`../store/README.md`](../store/README.md).

Disk: about 10.7 kB per chunk at 1024 dims, so ~535 MB for a 500k-line codebase.

---

## 7. Tuning the chunker

| Key | Default | Raising it means |
|-----|---------|------------------|
| `FINDERY_CHUNK_MAX_CHARS` | 2400 | fewer, larger excerpts: better context, less precision, more tokens per hit |
| `FINDERY_CHUNK_MIN_CHARS` | 120 | more aggressive merging of tiny neighbours |
| `FINDERY_CHUNK_OVERLAP_CHARS` | 200 | more redundancy across window boundaries |

2400 is the compromise for Rust (~660 tokens). A documentation-heavy repository
takes more. Any change re-chunks everything, which changes every content hash —
so the whole project re-embeds.

---

## 8. Multiple projects, multiple clients

One daemon serves every project and every client; the model is loaded once. The
marginal cost of indexing one more repository is a single `findery init`.

Point a whole team at a shared daemon with `FINDERY_SERVER_URL`, and set
`FINDERY_API_TOKEN` on both sides — every request then needs
`Authorization: Bearer …`, except `/healthz` so container health checks keep
working.

`FINDERY_JOB_CONCURRENCY` bounds simultaneous syncs so two clients cannot saturate
the embedding runtime between them.

---

## 9. When something looks wrong

| Symptom | First move |
|---------|-----------|
| `no indexed project contains …` | `findery init` here, or `-p <id>` |
| daemon does not answer | `make up-model` then `findery`; the error prints both lines |
| every search returns 502 | the embedding runtime is down — `findery health` |
| sync re-embeds everything | the recipe changed, or mtime is 0 on a mounted volume |
| search finds nothing that exists | not synced, or a filter is too strict — check `findery status` |
| hits cite unopenable paths | all-Docker mode without the matching `FINDERY_MOUNTS` entry |

`FINDERY_LOG=debug findery` for the full trace. `findery search --json` shows the
raw scores, which separates "ranked badly" from "not indexed".

The deeper diagnosis paths live in [`../index/README.md`](../index/README.md)
and [`../embed/README.md`](../embed/README.md).
