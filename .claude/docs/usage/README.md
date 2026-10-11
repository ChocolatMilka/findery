# Using it well

The quickstart in the README gets you searching. This page is the rest: every
knob, what it actually changes, and when reaching for it is the right move.

---

## 1. Two ways to run it

There is one thing to start, and it is the model. Everything else is a `findery`
command.

```bash
make install        # findery, finderyd, findery-mcp → ~/.cargo/bin
make up-model       # the model, in Docker
```

Without `make` — Windows — the same names work through
`.\tools\tasks.ps1 install` and `.\tools\tasks.ps1 up-model`.

Nothing else is on that list. The first `findery` command in a project starts the
index server in the background, silently, and every later command reuses it; the
one you would otherwise keep a terminal open for is not yours to manage. It sees
your real filesystem, so **any path on any disk is indexable** — no mount to
declare, no path translation. `FINDERY_WORKSPACES` is irrelevant here.

| Command | For |
|---------|-----|
| `findery doctor` | what is up, what is not, and the command for each |
| `findery health` | whether the index answers, and what it serves |
| `FINDERY_NO_SPAWN=1` | forbid the automatic start (service manager, container) |

`findery daemon start` / `stop` / `status` still exist for a service manager and
for debugging — they are hidden from `--help`, because a tool that starts itself
should not invite you to start it.

Only a loopback server is ever started automatically. An address pointing
elsewhere is someone else's host: failing to reach it is reported, never replaced
by a local index answering from nothing.

**Why a process at all?** Searching means comparing a query against every chunk,
from a matrix of normalized vectors held contiguously in RAM. For a large
codebase that is a few hundred megabytes; a fresh process would spend one to
three seconds rebuilding it from SQLite before it could compare anything. One
long-lived process builds it once and answers in milliseconds. That is the whole
reason, and it is why the process is not something you are asked to think about.

The container beside it is a different thing entirely: a stateless third-party
inference runtime that turns text into vectors and keeps nothing. It is never the
index.

### All-Docker

```bash
make up
```

The daemon cannot see your disk, so the CLI reads the files and sends them.
Nothing to mount, nothing to translate: `findery init` works from any directory
on any drive, exactly as in native mode.

This is also what makes a daemon on **another machine** possible — a team
server, a VM — which a daemon that has to read your filesystem never could.

### Pull or push, and who decides

Registration tests the path once: if the daemon can read it, it walks it
itself (pull); if it cannot, the client walks and pushes. Nobody has to
remember which mode a project is in.

| | Pull | Push |
|---|---|---|
| Who reads the files | the daemon | the client |
| Who watches | the daemon, in the background | `findery watch`, in the foreground |
| Bind mounts | one per root | none |
| Daemon on another machine | impossible | works |
| Chosen when | the daemon can read the root | it cannot, or `--push` |

`findery init --push` forces it — useful against a stale mount, or when the
daemon is going to move.

Both modes share the write path and the vector cache, so switching a project
from one to the other costs a walk and no embedding at all.

### Where things land

| What | Native | All-Docker |
|------|--------|------------|
| Model weights | volume `findery_hf-cache` | same |
| Bucket + vectors | `$FINDERY_DATA_DIR/bucket.db`, default `%LOCALAPPDATA%/findery` (or `~/.local/share/findery`) | volume `findery_findery-data` |
| Exports | `$FINDERY_DATA_DIR/exports` | `/data/exports` in that volume |

`finderyd --check` prints the configuration as actually resolved.

### Catalogs — more than one index per daemon

A **bucket** is one SQLite file holding projects, chunks, the vector cache and
the full-text index. A daemon serves as many as you create, and they are
completely independent: separate projects, separate vectors, separate files.

| Name | Path |
|------|------|
| the default one | `<data_dir>/bucket.db` |
| any other | `<data_dir>/buckets/<name>/bucket.db` |

```bash
findery bucket ls                    # what each holds, its recipe, its path
findery bucket create work           # an empty one, ready to take projects
findery bucket rm work --force       # projects and vectors go with it
```

Which bucket a project belongs to is chosen **once**, at registration:

```bash
findery init --bucket work
```

and never named again — every later command follows the project. A project's
bucket is not a column anywhere: it *is* which file holds its row, which is why
a project can never end up split across two.

`findery bucket rm` refuses while the bucket still holds projects unless
`--force` is passed, and it goes through the daemon because the daemon holds the
file open — on Windows a SQLite file a live process has open cannot be removed
at all.

**When to reach for one.** Two repositories that should not share a file: a
work index and a personal one, a client's code and your own. Or a model change:
a new recipe can go in a new bucket, leaving the old index intact and readable
until you are ready to move. What it is *not* for is search scoping — one
bucket holding many projects still searches them together, and `-p <id>` is
the lever for narrowing that.

### Settings live in a file, not in your shell

```bash
findery config set model qwen3.7-text-embedding
findery config show
findery config unset dim
findery config path
```

Written to `<data_dir>/config.toml`, validated before the write, and the daemon
is restarted so it takes effect. `model`, `url`, `key`, `dim`, `store`, `token`,
`bind`, `instruction` and `extensions` are short names for the common keys;
everything else takes its dotted path.

The precedence has three levels, narrowest last:

| | Read | Wins over |
|---|---|---|
| `/etc/findery/config.toml` | if it exists | nothing |
| `<data_dir>/config.toml` | when the first does not | the packaged file |
| `FINDERY_*` | always | both |

The environment staying on top is deliberate: one container image has to serve
every deployment. It is also why `findery config set` warns when a variable is
masking what it just wrote.

A daemon reads all of this **once, at boot**, and inherits the environment of
whatever started it — your shell, or the MCP client that launched
`findery-mcp`. That is why an MCP-launched daemon should take its settings from
the file, or from the `env` block of `.mcp.json`, never from a shell profile the
client may never read.

### Naming the daemon without an environment variable

The **daemon's** address (what it binds) and a **client's** address (what it
dials) are two halves of one deployment, so they live in the same file:

```bash
findery config set server https://findery.internal.example.com
findery config set token  <the bearer token it asks for>
```

`server` is the daemon address; `url` is the embedding endpoint. They are easy to
confuse and mean different things, which is why each has its own short name
rather than sharing one.

A client resolves its address in this order, narrowest last:

| Layer | How |
|---|---|
| the flag | `--server <url>` / `--token <t>`, on any command |
| the environment | `FINDERY_SERVER_URL` / `FINDERY_API_TOKEN` |
| the settings file | `server.url` / `server.api_token` |
| loopback | `http://127.0.0.1:8077`, when none of the above names one |

The point of the file layer is that on Windows naming a daemon otherwise costs a
`setx` and a new session, and on any platform it is invisible to a process
started from a shortcut. `findery config set server …` is a command that
persists, not an export that dies with the terminal.

One asymmetry worth knowing: **`findery-mcp` takes its address from the `env`
block of `.mcp.json`**, not from this file. Claude Code launches the bridge
rather than your shell, and an MCP client's `env` block is its native
configuration mechanism. Leave the block out for a loopback daemon; add
`FINDERY_SERVER_URL` to it when the daemon is elsewhere. The default both fall
back to is the same constant, so an unconfigured pair still meets.

---

## 2. Everyday commands

Every project-scoped command finds the project containing the current
directory. `-p <id>` overrides it, from anywhere.

```bash
cd D:/work/my-project
findery init                       # registers the current directory
findery sync                       # indexes it
findery search "where is retry handled"
findery status                     # files, chunks, vectors, model, disk
findery projects                   # all of them, current one marked *
```

| Command | What it does |
|---------|--------------|
| `findery init [PATH]` | registers a directory. `--id` to name it, `--ext rs,md,toml` to choose extensions, `--bucket <name>` to choose its index, `--sync` to index it immediately |
| `findery sync` | incremental. `--full` ignores the metadata and hash gates (never the vector cache). `--path src/a.rs` targets files. `--background` returns a job id |
| `findery bucket ls` / `create <name>` / `rm <name>` | the independent indexes this daemon serves. `rm` needs `--force` while one holds projects |
| `findery watch` | re-syncs on file changes, debounced. `--off` stops it |
| `findery doc add <path> --stdin` | indexes text with no file behind it — notes, decisions, generated docs |
| `findery doc rm <path>` | drops one document |
| `findery export --format binary` | vectors + metadata out of the index |
| `findery rm` | removes a project, its chunks and its vectors |
| `findery health` | what the daemon is actually serving |
| `findery jobs [id]` | background sync jobs |

`--json` on any command prints the daemon's answer verbatim, for scripting.

**`init` stops at registration.** Indexing is a separate, deliberate step, so a
repository can be claimed — and its bucket chosen — before any model is
reachable. `--sync` collapses the two.

---

## 3. Searching precisely

### Retrieval intent — the cheapest accuracy you will get

qwen3.7-text-embedding is instruction-aware: a task-tailored instruction buys 1-5%
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
FINDERY_EMBED_MODEL=qwen3.7-text-embedding   FINDERY_EMBED_DIM=1024   # the default
FINDERY_EMBED_MODEL=qwen3.7-text-embedding   FINDERY_EMBED_DIM=2560   # same model, widest vector
FINDERY_EMBED_BASE_URL=https://api.openai.com   FINDERY_EMBED_API_KEY=sk-…
```

Changing model or dimension opens a **new** cache namespace
(`recipe = "<model>:<dim>"`). A bucket refuses to mix two recipes, and the
refusal lands on the operation that would mix them — the sync, never the daemon
boot. That matters: a bucket built by another model still serves keyword search
and can still be deleted, and a daemon that refused to start would take both of
those down, including the `findery bucket rm` the message recommends.

The remedies, in order of how much they cost you: index the project into a
**new bucket** (`findery init --bucket <name>`, leaving the old index intact
and readable), point `FINDERY_DATA_DIR` at a fresh directory, or delete the
bucket and re-index. `findery bucket ls` names the conflict on the bucket
that carries it.

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
marginal cost of indexing one more repository is a `findery init` plus a
`findery sync`.

Point a whole team at a shared daemon with `findery config set server <url>`,
and `findery config set token <t>` on both sides — every request then needs
`Authorization: Bearer …`, except `/healthz` so container health checks keep
working.

**Catalogs split that sharing when it should be split.** One daemon, one model,
but several independent indexes: a work index and a personal one never share a
file, and a client pointed at one cannot search the other. What they do *not* do
is scope a single search — projects inside one bucket are searched together,
and `-p <id>` is the lever for that.

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
| a sync is refused, naming two recipes | that bucket was built by another model — `findery bucket ls` names it; index into a new bucket, or `findery bucket rm` it |
| search finds nothing that exists | not synced, or a filter is too strict — check `findery status` |
| search returns another project's code | it is in the same bucket — buckets split indexes, `-p <id>` narrows within one |
| hits cite unopenable paths | all-Docker mode without the matching `FINDERY_MOUNTS` entry |

`FINDERY_LOG=debug findery` for the full trace. `findery search --json` shows the
raw scores, which separates "ranked badly" from "not indexed".

The deeper diagnosis paths live in [`../index/README.md`](../index/README.md)
and [`../embed/README.md`](../embed/README.md).
