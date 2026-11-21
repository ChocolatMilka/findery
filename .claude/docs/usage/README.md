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

Windows also has a released `.msi`, for a machine with no Rust toolchain at all:

- **What it installs** — the three binaries into `C:\Program Files\findery`,
  and that directory appended to the system `PATH`.
- **Why the binaries go together** — `findery` resolves `finderyd` next to
  itself before consulting `PATH`: a CLI split from its server is a CLI talking
  to a build it never found.
- **What it closes first** — a running `finderyd`, before replacing it, since
  Windows will not overwrite a running executable and the daemon starts itself.

See [`packaging/windows/`](../../../packaging/windows/README.md).

Nothing else is on that list. The first `findery` command in a project starts the
index server in the background, silently, and every later command reuses it; the
one you would otherwise keep a terminal open for is not yours to manage.

It sees your real filesystem, so **any path on any disk is indexable** — no
mount to declare, no path translation. `FINDERY_WORKSPACES` is irrelevant here.

| Command | For |
|---------|-----|
| `findery doctor` | what is up, what is not, and the command for each |
| `findery health` | whether the index answers, and what it serves |
| `FINDERY_NO_SPAWN=1` | forbid the automatic start (service manager, container) |

`findery doctor` covers:

- every model profile
- what the server did on its own at boot (a bucket file renamed, or kept in
  place and why)
- every project with no vectors under its model yet

`findery daemon start` / `stop` / `status` still exist for a service manager and
for debugging — they are hidden from `--help`, because a tool that starts itself
should not invite you to start it.

Only a loopback server is ever started automatically. An address pointing
elsewhere is someone else's host: failing to reach it is reported, never replaced
by a local index answering from nothing.

**Why a process at all?** Searching means comparing a query against every chunk,
from a matrix of normalized vectors held contiguously in RAM. For a large
codebase that is a few hundred megabytes; a fresh process would spend one to
three seconds rebuilding it from SQLite before it could compare anything.

One long-lived process builds it once and answers in milliseconds. That is the
whole reason, and it is why the process is not something you are asked to think
about.

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
| Bucket + vectors | the default bucket's files, `$FINDERY_DATA_DIR/bucket.<model>-<tag>.db`, default `%LOCALAPPDATA%/findery` (or `~/.local/share/findery`); a project's own under its `.findery/` | volume `findery_findery-data` |
| Exports | `$FINDERY_DATA_DIR/exports` | `/data/exports` in that volume |

`finderyd --check` prints the configuration as actually resolved.

### The manifest — `.findery/config.toml`

`findery init` writes one, and every later command reads it before asking
anyone anything. That is what makes a project a thing you can read, commit, and
carry: a cloned repository arrives knowing which index it belongs to, and a
command run three directories down finds it by walking up.

The MCP bridge finds its project the same way, by the same code
(`findery_client::resolve`):

1. `-p` / `project`;
2. this file, walked up;
3. the deepest registered root holding the directory.

Running `findery init` again re-sends everything the manifest declares — an
emptied `[scopes]` or a removed `instruction` included — and the daemon applies
it.

Any other registration (the MCP `projects {action: "register"}`, a script
posting a root) is a **merge**: what it leaves out is kept, bucket included.

```toml
project = "axle"

[bucket]
name = "axle"
path = ".findery/bucket.db"

[scopes.api]
paths = ["crates/server/**", "crates/client/**"]
langs = ["rust"]
```

| Key | Meaning |
|-----|---------|
| `project` | the id it is registered under. `init` derives it from the directory name |
| `bucket.name` | which index it belongs to — the identity, and what makes a bucket shareable |
| `bucket.path` | where that index's file is. Relative paths resolve against the project root, so a committed manifest travels |
| `scopes` | named restrictions a search can ask for by name — see below |
| `instruction` / `task` | the retrieval intent this project's queries get by default |
| `model` | the model profile this project is pinned to (`findery init --model <alias>`); absent, it follows the default model (§5) |

`init` also writes `.findery/.gitignore` holding `*`, so the index is never
committed by accident. An existing one is left alone — that file is yours.

**Defaults are the ordinary case.** With no `[bucket]` at all, the bucket lives
inside the project at `.findery/bucket.db` and is named after the project: your
index travels with your code, and nothing is shared that you did not ask to
share. `findery init` writes that back explicitly, so the file says what is
true.

**`init` re-reads before it writes.** Everything except the bucket survives a
second `findery init`: your scopes, your instruction and your model pin were
written by hand, and re-registering must not be a way to lose them. `--model`
replaces the pin; nothing else does.

### Buckets — one index, one file per model

A **bucket** is an index: projects, chunks, the vector cache and the full-text
index. A daemon serves as many as it is pointed at, and they are completely
independent. On disk a bucket is **one SQLite file per model** that indexed
it, named after its base path:

| Where | Base path | Files |
|-------|-----------|-------|
| inside the project *(the default)* | `<project>/.findery/bucket.db` | `.findery/bucket.<model>-<tag>.db` |
| a named, shared one | `<data_dir>/buckets/<name>/bucket.db` | `buckets/<name>/bucket.<model>-<tag>.db` |
| anywhere you say | whatever `bucket.path` holds | beside it, same pattern |

`<model>` is readable (`harrier-oss-v1-0-6b`), `<tag>` is 8 characters that
tell two vector spaces apart.

A bucket file from before per-model files — `bucket.db` itself — is renamed to
its model's name at the next start. When it cannot be (another process holds
it), it keeps serving where it is and `findery doctor` says so.

```bash
findery bucket ls                         # each bucket, and one line per file in it
findery bucket create work                # a named one, ready to take projects
findery bucket rm work --force            # projects and vectors go with it
findery bucket rm axle --model qwen3      # only that model's file; the projects stay
```

```text
* default  C:/Users/u/AppData/Local/findery/bucket.db
    no file yet — the first sync under a model creates its file
  axle     D:/work/axle/.findery/bucket.db
    harrier-oss-v1-0-6b-zvpmtsdf  harrier-oss-v1-0.6b:1024                  1 project  812 chunks   3.1 MB  serving
    bucket.db                     text-embedding-qwen3-embedding-0.6b:1024  1 project  790 chunks   2.9 MB  legacy

* default bucket · serving = the file of the default model (`findery model ls`)
```

The status column marks:

- `serving` — the file the default model reads and writes
- `legacy` — a `bucket.db` that could not be renamed yet
- `conflict` — a file holding another model's vectors than its name says, with
  the repair on the next line

**A bucket is global when you point two projects at the same one** — that is
all "shared" means here. A team index on a shared drive is one file and one
name, whatever machine reads it:

```toml
[bucket]
name = "team"
path = "D:/shared/team.db"
```

Nothing is written down about *which project lives in which bucket*: that fact
is which file holds the project's row, and it stays in exactly one place.

The daemon does keep a small `buckets.toml` in the data directory, but only as a
discovery index — it records where a bucket's *file* is, because a path cannot
be derived from a name. Named buckets are still found by scanning, so losing
that file costs only the out-of-place ones.

The choice is made **once**, at registration, and never named again.

`findery bucket rm` refuses while the bucket still holds projects unless
`--force` is passed. It goes through the server because the server holds the
file open — on Windows a SQLite file a live process has open cannot be removed
at all.

`--model <alias>` needs no `--force`:

- it deletes that model's file only;
- the projects stay on their other models;
- the bucket's last file is refused (that is the bucket — remove it without
  `--model`).

**When to reach for a separate bucket.** Two repositories that should not share
a file: a work index and a personal one, a client's code and your own.

A model change is *not* a reason: every bucket already keeps one file per model
(§5). Nor is search scoping — projects in one bucket still search together, and
`-p <id>` narrows that.

### Scopes — a named restriction on the same index

A scope is declared in the manifest and asked for by name. It is a **filter
applied before scoring**, on the same index: never a second bucket, never a cut
applied after the ranking.

```toml
[scopes.api]
paths = ["crates/server/**", "crates/client/**"]
langs = ["rust"]
```

```bash
findery search --scope api "where is the retry policy"
findery search --scope api --lang markdown "…"   # the flag wins for that dimension
```

The merge is **per dimension**: a flag you pass replaces the scope's value for
that dimension, and a dimension you leave empty keeps the scope's. Intersecting
them instead would make `--scope api --lang markdown` a search that matches
nothing.

A scope name nobody declared is refused, and the refusal lists the ones that
exist. A malformed glob is refused earlier still — when the manifest is read,
which is while its author is looking at the file, and never at search time.

### Settings live in a file, not in your shell

```bash
findery config set model qwen3.7-text-embedding
findery config show
findery config unset dim
findery config path
```

Written to `<data_dir>/config.toml`, validated before the write, and the daemon
is restarted so it takes effect.

`model`, `url`, `key`, `dim`, `store`, `token`, `bind`, `instruction` and
`extensions` are short names for the common keys; everything else takes its
dotted path.

Three sources, the last one winning. The two files never stack: the daemon
reads one or the other.

| Source                     | Read                          | Wins over             |
|----------------------------|-------------------------------|-----------------------|
| `/etc/findery/config.toml` | if it exists                  | the built-in defaults |
| `<data_dir>/config.toml`   | only when the first does not  | the built-in defaults |
| `FINDERY_*`                | always                        | whichever file was read |

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

A client takes its address from the first layer that names one:

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

> **Note** — one asymmetry worth knowing: **`findery-mcp` takes its address
> from the `env` block of `.mcp.json`**, not from this file.

Claude Code launches the bridge rather than your shell, and an MCP client's
`env` block is its native configuration mechanism.

- Leave the block out for a loopback daemon.
- Add `FINDERY_SERVER_URL` to it when the daemon is elsewhere.

The default both fall back to is the same constant, so an unconfigured pair
still meets.

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
| `findery init [PATH]` | registers a directory. `--id` to name it, `--ext rs,md,js,py` to choose extensions, `--bucket <name>` to choose its index, `--model <alias>` to pin its model, `--sync` to index it immediately |
| `findery sync` | incremental. `--full` ignores the metadata and hash gates (never the vector cache). `--path src/a.rs` targets files. `--background` returns a job id instead of waiting |
| `findery search "…"` | hybrid search of the current project. `--task <name>` picks a retrieval intent, `--instruct` writes your own — §3 |
| `findery jobs [id]` | what each background job is doing, and where a running one has got to |
| `findery bucket ls` / `create <name>` / `rm <name>` | the independent indexes this daemon serves, one line per model file. `rm` needs `--force` while one holds projects; `rm <name> --model <alias>` deletes one model's file |
| `findery model ls` / `show [alias]` / `use <alias>` | the model profiles, which one is the default, which projects use each — and the switch (§5) |
| `findery watch` | re-syncs on file changes, debounced. `--off` stops it |
| `findery doc add <path> --stdin` | indexes text with no file behind it — notes, decisions, generated docs |
| `findery doc rm <path>` | drops one document |
| `findery export --format binary` | vectors + metadata out of the index |
| `findery detach` | takes a project off the server and **keeps** its index: the files are closed, unlocked, and not reopened at the next start. `findery init` in the project attaches it again, instantly |
| `findery rm` | **deletes** a project, its chunks and its vectors — and its bucket's files when no other project shares them. The manifest stays |
| `findery health` | whether the index answers, and what it serves |

### `detach` or `rm`

Both stop the project's running sync and its watcher first, and both say which
files they touched.

```text
$ findery detach
detached axle — its index stays on disk
  closed   D:/work/axle/.findery/
             bucket.harrier-oss-v1-0-6b-zvpmtsdf.db
             bucket.text-embedding-qwen3-embedding-0-6b-47m32eui.db
  back     `findery init` in D:/work/axle re-attaches it; nothing is re-indexed

$ findery rm
removed axle — its chunks, vectors and bucket files
  deleted  D:/work/axle/.findery/
             bucket.harrier-oss-v1-0-6b-zvpmtsdf.db
             bucket.text-embedding-qwen3-embedding-0-6b-47m32eui.db
  kept     .findery/config.toml — `findery init` registers the project again
```

- **`detach`** — reach for it to move, back up or delete the files yourself:
  after it, no findery process holds them.
- **`rm`** — reach for it to be done with the index.

A bucket another project still uses stays open on `detach` and keeps its files
on `rm`; the answer names who shares it.

Neither asks for confirmation, as `rm` never did; `bucket rm` is the one that
wants `--force`.

Either verb can answer `busy`:

- **A sync that does not stop in time** — within `server.drain_timeout_secs`
  (`FINDERY_DRAIN_TIMEOUT_SECS`, 15 s), either verb answers `busy`, naming the
  job, with nothing changed.
- **A file another process holds** — `rm` answers `busy` naming the file; the
  next start finishes the removal before anything opens it.

### Watching a first index

A first pass over a large repository takes minutes, so `findery sync` draws on
**stderr** while it runs. While the walk runs, a count:

```text
⠹ scanning… 4 120 files
```

then, once the plan is known, up to two bars — each drawn once it means
something:

```text
files =========>                   96/312 · 6 888 unchanged
embed ===============>             1 541/2 300 · 5 937 cached · 38/s · ETA 0:20
```

- **`files`** — counts the files this pass reads. Gate 1 has already set aside
  the unchanged ones, before the first read, so `312` is a count and not an
  estimate, and a bar over an unchanged tree does not start at 96 %.
- **`embed`** — counts the chunks the vector cache did not have, and moves with
  every batch the runtime answers, not once per flush. Its ETA covers the
  misses found so far.

Three cases draw differently:

- **A sync already running on the project** — `sync` follows **that** one and
  says so — `attached to the running sync (started 12s ago)` — instead of
  appearing to jump.
- **A sync waiting behind others** — reads `queued — waiting for a slot`.
- **A project in push mode** — draws the same board from its own walk: every
  walked file is pending (the client has no gate 1), and the embedding count
  has no denominator, since only the daemon knows the misses.

The board only exists when someone is watching — a terminal on stderr and no
`--json`. Piped or redirected, the run prints the same one-line summary it
always did, which is what a script or a CI log can grep for.

Either way the run IS a job on the daemon, followed poll by poll: the CLI never
holds one request open for the length of a sync, because a client timeout would
then abort the indexing mid-write. `--background` returns the job id instead of
following it.

`--json` on any command prints the daemon's answer verbatim, for scripting.

**`init` stops at registration.** Indexing is a separate, deliberate step, so a
repository can be claimed — and its bucket chosen — before any model is
reachable. `--sync` collapses the two.

---

## 3. Searching precisely

### Retrieval intent — the cheapest accuracy you will get

`qwen3.7-text-embedding` is instruction-aware: a task-tailored instruction buys
1-5% retrieval accuracy for the cost of one prefix on a string that was going to
be embedded anyway.

```bash
findery search "QueryTask" --task symbol            # you have a name, you want its definition
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

`--task` also takes their aliases (`--task callers`). An unknown task,
`--lang` or `--kind` is refused with the list of valid ones — never dropped.

`--instruction "…"` (`--instruct`) overrides the preset with free-form text.
Describe the TASK, not the query — restating the query duplicates signal the
query already carries and performs worse than the default.

**The model reads a pair.** Qwen and Harrier are handed `Instruct: <the
task>\nQuery: <your question>` — the default `query_template`; a model with
another format gets its own (§5). The two halves are separate all the way from
the CLI or the MCP tool to the embedding request.

The whole precedence, resolved once in L0 so no transport can invent its own
ordering:

```text
--instruction  >  --task  >  the project's manifest  >  the daemon's
                                                          embedder.query_instruction
```

A manifest can therefore give a project its own default:

```toml
instruction = "Given a question about axle, retrieve the passage that answers it."
```

### Mode

```bash
findery search "E0432" --mode keyword     # exact identifier: skip the semantics
findery search "how do we avoid re-embedding" --mode vector
```

`hybrid` (the default) fuses both by rank and beats either one alone on code.
Reach for `keyword` when you know the exact string, `vector` when the wording in
the code is nothing like the wording in your head.

When the vector half cannot run — the project was never synced under the model
it now uses, or that model's runtime is down — `hybrid` still answers, from the
keyword half, and prints one `findery: keyword results only: …` line on
**stderr** naming the model and the fix. stdout (and `--json`) carries the hits
as always. `--mode vector` fails instead.

### Filters — applied before scoring, not after

```bash
findery search "cache" --path 'crates/store/**' --lang rust --kind method --symbol cache
```

| Filter | Takes |
|--------|-------|
| `--path` | globs |
| `--lang` | one of `rust markdown toml javascript python php json axle java kotlin go other` |
| `--kind` | one of `file function method struct enum trait impl module macro test const doc_section text` |
| `--symbol` | a substring the symbol must contain |

A malformed glob or an unknown value is rejected, never silently matched
against nothing.

### Token budget

The two levers that matter, in order:

```bash
findery search "…" --max-chars 600    # per-excerpt ceiling — the strongest lever
findery search "…" -k 3               # fewer excerpts
```

`-k` defaults to the daemon's `search.default_k` (10).

`k=8` at 1200 characters is roughly 2,500-3,500 tokens for a full answer
(measured: `.claude/docs/mcp/README.md`). Truncation happens on a line boundary
and is marked `… [truncated]`.

The wait a sync request may ask for (`wait_ms`) is capped by
`server.max_sync_wait_ms` (120000).

---

## 4. Choosing what gets indexed

Per project, at `findery init` or by re-registering:

```bash
findery init --ext rs,md,toml,yml,sql
```

Five independent levers, applied in this order:

| Lever | Default | Effect |
|-------|---------|--------|
| `extensions` | `rs`, `md`, `toml`, `js`, `mjs`, `cjs`, `ts`, `py`, `php`, `json`, `axle`, `java`, `kt`, `kts`, `go` | empty means everything that survives the rest |
| `include` | none | globs; narrows to these |
| `exclude` | build output + secrets | globs; subtracts |
| `follow_gitignore` | true | honours `.gitignore` even outside a git repo |
| `include_hidden` | true | descends into `.github/`, `.cargo/`, `.claude/` |

Dot-directories are indexed **by default**, because `.github/workflows`,
`.cargo/config.toml` and design notes under `.claude/` are exactly what people
ask about.

The default `exclude` list is what keeps credentials out: `.env*`, `.ssh/`,
`.aws/`, `.gnupg/`, `.netrc`, `.npmrc`, `*.pem`, `*.p12`, `id_rsa*`,
`id_ed25519*`. Drop a pattern if you want that file indexed.

Files that are not valid UTF-8, or above `max_file_bytes` (2 MB), are skipped
and **reported** in `warnings` — never dropped in silence.

**Chunking quality differs by language.**

| Files | Chunker |
|-------|---------|
| `.rs`, `.js`/`.ts`, `.py`, `.php`, `.axle`, `.java`, `.kt`/`.kts` and `.go` | a syntactic chunker (one chunk per item, with symbol and breadcrumb) |
| `.md` | a heading-based one |
| `.json` | answers as one chunk while it fits `max_chars`, then splits per top-level member, each carrying its key |
| `.toml` and everything else | fall back to line windows: usable, but with no symbol |

Adding a real chunker for a language is:

- a `Profile` in `findery-chunk` — its lexicon, its keyword table;
- one line in `Registry`;
- one entry in the default `extensions`;
- one arm in `Lang`.

See [`../chunk/README.md`](../chunk/README.md).

**Line endings do not matter.** A `\r\n` is read as `\n` before chunking, so a
Windows checkout and a Linux one of the same commit give the same chunks and
the same content hashes: cloning a repository on the other OS costs no
embedding. A file indexed before this rule re-embeds its chunks once, the next
time it is chunked again (an edit, or `findery sync --full`).

Global defaults for every project: `FINDERY_EXTENSIONS=rs,md,js,py` and
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

### Model profiles — several models, one picked

The configuration file can name several models. `[embedder]` is the implicit
profile `embedder`; each `[models.<alias>]` states only what differs from it:

```toml
default_model = "harrier"             # which profile the daemon embeds with

[embedder]
base_url = "http://localhost:1234"    # LM Studio
model = "text-embedding-qwen3-embedding-0.6b"
dim = 1024

[models.harrier]
model = "harrier-oss-v1-0.6b"
```

| Setting | Variable | What it does |
|---|---|---|
| `default_model` | `FINDERY_MODEL` | the profile the daemon embeds with; absent or empty = `[embedder]`; an unknown alias refuses the boot and lists the known ones |
| `[models.<alias>]` | — | a profile merged over `[embedder]`; a typo in it is refused like one in `[embedder]` |
| `embedder.weights` | `FINDERY_EMBED_WEIGHTS` | the canonical name of the weights, replacing `model` in the recipe — two runtimes serving the same weights under two names then share vectors. A GGUF quantization is not the same weights |
| `embedder.query_template` | `FINDERY_EMBED_QUERY_TEMPLATE` | how the model wants a query; must contain `{query}`, `{instruction}` optional. Default `Instruct: {instruction}\nQuery: {query}` |
| `embedder.document_template` | `FINDERY_EMBED_DOCUMENT_TEMPLATE` | how it wants a document; must contain `{text}`. Default `{text}` |

In a variable, a literal `\n` stands for a newline.

A profile pointing at another `base_url` does **not** inherit `api_key` or the
auth headers, and one naming another `model` does not inherit `weights` or the
templates.

A nomic-style model is pure configuration:

```toml
[models.nomic]
base_url = "http://localhost:11434"
model = "nomic-embed-text:latest"
dim = 768
query_template = "search_query: {query}"
document_template = "search_document: {text}"
```

`config/findery.toml` ships `harrier` and `qwen3` for LM Studio, inert until
selected. An alias is letters, digits, `-` and `_` — no `.`, because it is one
segment of a dotted setting: `findery config set models.qwen3.dim 1024`.

### Switching model — `findery model`

```text
$ findery model ls
   ALIAS     MODEL                                 DIM  TAG       STATUS  USED BY
   embedder  text-embedding-qwen3-embedding-0.6b  1024  47m32eui  idle    -
*  harrier   harrier-oss-v1-0.6b                  1024  zvpmtsdf  ready   axle, smalt
   qwen3     text-embedding-qwen3-embedding-0.6b  1024  47m32eui  idle    docs (pinned)

* default — `findery model use <alias>` switches it now, re-embedding nothing
```

| Command | What it does |
|---|---|
| `findery model ls` | every profile: its model, width, recipe tag, `ready` / `idle` (not used yet) / `down`, and the projects using it now, pinned ones marked |
| `findery model show [alias]` | one profile — the default when omitted: recipe, the file name its vectors live in, and why it is `down` |
| `findery model use <alias>` | makes it the default **at once** — the next command uses it, no restart — and saves `default_model` in `<data_dir>/config.toml` so the next start keeps it |
| `findery init --model <alias>` | pins one project to a profile, whatever the default; written to its manifest as `model = "…"` |

An alias nobody defined is refused before anything changes, naming the known
ones and the file to add `[models.<alias>]` to.

Each model's vectors live in their own file of every bucket, so a switch moves
nothing:

- under a model a project was never synced with, a search answers keyword hits
  with a `keyword results only` line until `findery sync` builds that model's
  file;
- switching back to a model already synced costs nothing at all.

`FINDERY_MODEL` still wins over the saved `default_model` at the next start,
and `model use` warns when it is set.

Changing model or dimension opens a **new** cache namespace
(`recipe = "<model>:<dim>"`, with `weights` standing in for the model when set,
and `|doc=<fingerprint>` added by a non-default `document_template`). With no
`weights` and no `document_template`, the recipe is exactly what it always
was, so nothing already embedded is lost.

A bucket refuses to mix two recipes, and the refusal lands on the operation
that would mix them — the sync, never the daemon boot.

That matters: a bucket built by another model still serves keyword search and
can still be deleted, and a daemon that refused to start would take both of
those down, including the `findery bucket rm` the message recommends.

A model switch never meets that refusal: each recipe has its own file. What
does is a file whose content contradicts its name — renamed by hand, or a tag
collision — and `findery bucket ls` names the conflict on the file that carries
it, with its repair: rename the file back, or `findery bucket rm` and re-index.

At boot the daemon sends one throwaway input and compares the returned width to
`FINDERY_EMBED_DIM`. A mismatch is fatal — half a corpus at the wrong width is
silently unsearchable, and this is the only cheap moment to catch it.

Throughput:

- `FINDERY_EMBED_BATCH_SIZE` — inputs per request.
- `FINDERY_EMBED_CONCURRENCY` — batches in flight.

Raising them past what the runtime can absorb only lengthens the queue.

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
| `FINDERY_CHUNK_MAX_CHARS` | 2400 | fewer, larger excerpts: better context, less precision, more tokens per hit. A hard ceiling in bytes: even a one-line minified file is cut to it |
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

**Buckets split that sharing when it should be split.** One server, one model,
but as many independent indexes as there are bucket files: a work index and a
personal one never share a file, and a client pointed at one cannot search the
other.

What a bucket does *not* do is scope a single search — projects inside one
bucket are searched together, and `-p <id>` is the lever for that. Scoping one
search is what a [scope](#scopes--a-named-restriction-on-the-same-index) is for.

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
| a sync is refused, naming two recipes | a bucket file holds another model's vectors than its name says — `findery bucket ls` names it and its repair |
| `keyword results only: …` after `findery model use` | that project was never synced under the new model — `findery sync`; `findery doctor` lists every such project |
| `busy: …` on `detach` / `rm` | a sync did not stop in time, or another process holds the file — the message names which; nothing was changed |
| search finds nothing that exists | not synced, or a filter is too strict — check `findery status` |
| search returns another project's code | it is in the same bucket — buckets split indexes, `-p <id>` narrows within one |
| hits cite unopenable paths | all-Docker mode without the matching `FINDERY_MOUNTS` entry |

`FINDERY_LOG=debug findery` for the full trace. `findery search --json` shows the
raw scores, which separates "ranked badly" from "not indexed".

The deeper diagnosis paths live in [`../index/README.md`](../index/README.md)
and [`../embed/README.md`](../embed/README.md).
