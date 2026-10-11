# findery

**Local semantic index for codebases.** One embedding model, loaded once, in
the background. Multiple projects, multiple clients. Rust API, HTTP API,
MCP API — so it plugs directly into Claude Code.

- 🦀 100% Rust, a five-layer workspace
- 🔒 `qwen3.7-text-embedding` by default, **or** a fully local runtime, **or** any remote
  provider: nothing about the model is hardcoded
- ⚡ Three-gate incremental sync: re-syncing an unchanged repo costs
  **zero embeddings**
- 🔍 Hybrid search: vector + BM25, rank fusion
- 🗄️ Pluggable vector store: SQLite (exact, no service) or Qdrant
  (local vector database, HNSW)
- 📤 JSONL / JSON / raw `float32` export — you're never locked in
- 📡 Pull or push: the daemon reads your files, or the CLI sends them — so the
  daemon can run in a container, or on another machine, with nothing mounted
- 🧪 206 tests, clean `clippy -D warnings`, layer watchdog

---

> **New here?** [`docs/getting-started.md`](docs/getting-started.md) walks the
> whole setup step by step, with sizing tables and a troubleshooting section.
> **Handing it to a team?** [`docs/deploying.md`](docs/deploying.md).

## Getting started

```bash
git clone <this-repo> && cd findery
make install        # findery, finderyd, findery-mcp → ~/.cargo/bin
make up-model       # the Qwen3 model in Docker (~1.2 GB on first launch)
```

**On Windows**, where there is no `make`, the same task list is a PowerShell
script — same names, same commands:

```powershell
.\tools\tasks.ps1 install
.\tools\tasks.ps1 up-model
.\tools\tasks.ps1 help        # every task
```

Then, in **any project, on any disk**:

```bash
cd D:/work/my-project
findery init                              # registers the current directory
findery sync                              # indexes it
findery search "where is retry handled"   # the project is inferred from the directory
```

No path to type, no mount to declare, **and no daemon to start** — the first
command notices nothing is listening and starts `finderyd` in the background.
Nothing to keep open.

`init` and `sync` are two steps on purpose: a repository can be registered and
its bucket chosen **before** any model is reachable. `findery init --sync`
does both in one go when you are ready.

Run `findery doctor` whenever something looks wrong: it names what is missing
and the command that fixes it.

```bash
findery projects        # your projects, the current one marked with a *
findery status          # files, chunks, vectors
findery sync            # after coding — near instant
findery watch           # or let it resync on its own
findery search -p axle "retry"    # target another project from anywhere
findery bucket ls      # the independent indexes this daemon serves
```

### The two ways to run this

|  | Native daemon *(default)* | All in Docker |
|---|---|---|
| Commands | `make up-model` + `findery` | `make up` |
| What's containerized | the model alone | model + daemon |
| Who reads your files | the daemon | **the CLI, and pushes them** |
| Indexable projects | anywhere, any disk | anywhere, any disk |
| Bind mounts | none | none |
| Watching | background | `findery watch`, foreground |
| Who it's for | a dev machine | a shared server, a CI, a remote daemon |

The daemon is a 5 MB Rust binary with an embedded SQLite — containerizing it
on a dev machine gains nothing and costs the mount boundary. What deserves
Docker is the model.

The all-Docker mode needs no mounts either: when the daemon cannot see a
directory, the CLI reads it and sends the contents. That is also what lets the
daemon live on **another machine** — `findery config set server <url>` and index
normally.

### Naming a daemon

The address lives in the same settings file as everything else, so a client
finds its daemon without an environment variable:

```bash
findery config set server https://findery.internal.example.com
findery config set token  <the bearer token it asks for>
```

Precedence, narrowest last: `--server` / `--token`, then `FINDERY_SERVER_URL` /
`FINDERY_API_TOKEN`, then the settings file, then loopback. The environment
stays on top so one container image can serve every deployment; `findery config
set` warns when a variable is masking what it just wrote.

### More than one index

A daemon can serve several **buckets** — independent SQLite files, each with
its own projects and vectors. Useful when a work repository and a personal one
should not share a file, or when a client must not search another team's code.

```bash
findery bucket create work
findery init --bucket work          # this project lives in `work`
findery bucket ls                   # what each one holds, and its path
findery bucket rm work --force      # projects and vectors go with it
```

The bucket is chosen once, at registration: every later command follows the
project, so nobody has to remember which index a directory belongs to.

## Plugging into Claude Code

`.mcp.json` at the root of the project to index:

```json
{
  "mcpServers": {
    "findery": {
      "command": "C:/Users/users/RustroverProjects/findery/target/release/findery-mcp"
    }
  }
}
```

Eight tools appear, including `search_code`, which becomes the agent's first
reflex for locating code — in place of `glob` + `grep` + three `Read`.

The bridge is launched by Claude Code rather than by your shell, so it takes its
daemon address from the `env` block of this file — add
`"env": { "FINDERY_SERVER_URL": "https://findery.internal.example.com" }` when
the daemon is not on loopback. Leave it out for a local one.

Details and the Docker variant: [`.claude/docs/mcp/README.md`](.claude/docs/mcp/README.md).

## From another Rust project

```toml
[dependencies]
findery-client = { path = "../findery/crates/client" }
findery-core   = { path = "../findery/crates/core" }
```

```rust
use findery_client::Client;
use findery_core::{ids::ProjectId, model::SearchRequest};

let client = Client::from_env()?;              // FINDERY_SERVER_URL / FINDERY_API_TOKEN
let project = ProjectId::new("my-project")?;
for hit in client.search(&SearchRequest::new(project, "where is retry handled")).await? {
    println!("{}  {:.3}\n{}", hit.chunk.locator(), hit.score, hit.chunk.text);
}
```

`findery-client` only depends on `findery-core` and `reqwest`: no SQLite, no walker, no
chunkers.

---

## Choosing your model

Nothing is hardcoded. Three examples, all in `.env`:

```bash
# Local, provided (default)
FINDERY_EMBED_MODEL=qwen3.7-text-embedding   # or: findery config set model …
FINDERY_EMBED_DIM=1024

# A local runtime you already have (Ollama, llama.cpp, LM Studio, vLLM…)
FINDERY_EMBED_BASE_URL=http://host.docker.internal:11434
FINDERY_EMBED_MODEL=<the name your local runtime reports>

# A remote provider
FINDERY_EMBED_BASE_URL=https://api.openai.com
FINDERY_EMBED_MODEL=text-embedding-3-large
FINDERY_EMBED_DIM=3072
FINDERY_EMBED_API_KEY=sk-...
```

## Choosing your vector store

| Backend | Search | Service | When |
|---------|-----------|---------|-------|
| `sqlite` (default) | exact in-RAM scan — **12 ms at 50k chunks**, 50 ms at 200k | none | up to ~150k chunks |
| `qdrant` | HNSW, filters, int8 quantization | a local container | beyond that, or a shared index |

The numbers are reproducible: `make bench`. The full reasoning, including why
an **embedded** ANN index was measured and then ruled out, is in
[`.claude/docs/store/README.md`](.claude/docs/store/README.md).

---

## What this changes

Without an index, locating something in an unfamiliar codebase costs one
`glob`, three to six `grep` and three to eight `Read` — commonly **25,000 to
60,000 input tokens**. With `search_code`, the same question costs one call
and **2,500 to 3,500 tokens**, plus one targeted `Read` when the whole file
is really needed.

Exploration typically accounts for 40 to 60% of the input tokens of an agent
session on a codebase it doesn't know. Cutting that share by ~70% gives a
**net gain on the order of 30 to 45% on the session**, and just as much
latency saved — which often matters more.

The assumptions and the full calculation are detailed further down in
[ESTIMATE](#estimate-gains-and-savings).

---

## Architecture

```
 L4   findery-mcp (MCP stdio)      findery-cli (`findery`)        the edges
 L3   findery-server (axum)                              HTTP transport
 L2   findery-index                                      sync, search, jobs, watch
 L1   findery-chunk   findery-embed   findery-store   findery-client    one responsibility each
 L0   findery-core                                       vocabulary, zero I/O
```

Dependencies flow down, data flows back up through `Result`. Verified by
`make layers`.

| Page | Contents |
|------|---------|
| [`usage/`](.claude/docs/usage/README.md) | every flag, every knob, and when to reach for it |
| [`overview/pipeline.md`](.claude/docs/overview/pipeline.md) | from file to cited hit |
| [`overview/layers.md`](.claude/docs/overview/layers.md) | the law of the layers |
| [`chunk/`](.claude/docs/chunk/README.md) | Rust and Markdown chunking |
| [`embed/`](.claude/docs/embed/README.md) | models, Qwen3 instruction, providers |
| [`store/`](.claude/docs/store/README.md) | bucket, backends, measurements, export |
| [`index/`](.claude/docs/index/README.md) | sync, hybrid search, jobs, watch |
| [`server/`](.claude/docs/server/README.md) | HTTP API reference |
| [`mcp/`](.claude/docs/mcp/README.md) | MCP tools and setup |

Two pages sit outside that set, written for readers rather than contributors:
[`docs/getting-started.md`](docs/getting-started.md) and
[`docs/deploying.md`](docs/deploying.md) — the latter for handing findery to a
team.

---

## Development

```bash
make check     # fmt-check + clippy -D warnings + test + layers + sizes
make test      # 206 tests
make bench     # vector scan measurement
```

Windows: `.\tools\tasks.ps1 check` — it needs Git for Windows, because the two
watchdogs are shell scripts and it finds that bash rather than WSL's.

The repo's rules: [`.claude/rules/CONVENTIONS.md`](.claude/rules/CONVENTIONS.md).
Open work: [`.claude/roadmap/README.md`](.claude/roadmap/README.md).

---

## ESTIMATE — gains and savings

> Orders of magnitude, with the assumptions stated. Recalibrate on your own
> sessions: `/cost` in Claude Code gives the real measurement.

### Cost of an exploration, without and with the index

| | Without an index | With `search_code` |
|---|---|---|
| Tool calls | 1 glob + 3-6 grep + 3-8 read | 1 search + 0-2 targeted read |
| Input tokens | 25k – 60k | 3k – 9k |
| Round trips | 8 – 15 | 2 – 4 |
| Perceived latency | 30 – 90 s | 3 – 8 s |

### Over a full session

Assuming exploration accounts for 50% of the input tokens of a work session
on an unfamiliar codebase, and that 70% of it is removed:

| Session | Input without index | Input with index | Savings |
|---------|-------------------|-------------------|----------|
| Short (1 question) | ~80k | ~50k | ~37% |
| Medium (a feature) | ~500k | ~325k | ~35% |
| Long (refactor) | ~2M | ~1.3M | ~35% |

At **$5 / M input tokens** (Claude Opus 5, API pricing), an average session
goes from about $2.50 to $1.60 of input cost, i.e. **~$0.90 saved per
session**. Ten sessions a day, twenty days a month: **~$180/month** per
developer. On a flat subscription, the gain isn't monetary but shows up in
quota: that many more sessions before hitting the limit.

Real side effect: fewer reads of never-before-seen files means a more stable
context prefix, hence a **better cache hit rate** — cached reads are billed
at 10% of the input rate.

### Productivity

The token savings aren't the main gain. What really changes:

- **Time to locate** drops from tens of seconds to a few seconds. Over
  20 to 40 lookups a day, that's 15 to 30 minutes.
- **First-try success rate** goes up: the agent finds semantically relevant
  code that a grep on the wrong keyword would have missed.
- **The context window stays useful longer**: fewer whole files swallowed,
  so fewer mid-task compactions.
- **The marginal cost of a new project is zero**: the model is already
  warm, indexing one more repo is a single command.

### Running cost

| Item | Order of magnitude |
|-------|-------------------|
| First indexing, 500k lines, CPU | 10 – 25 min |
| Same, GPU | 1 – 3 min |
| Re-sync, nothing changed | < 1 s, **zero embeddings** |
| Re-sync after 10 modified files | 2 – 10 s |
| Daemon RAM, 50k chunks at 1024 dims | ~250 MB |
| Disk, bucket + cache | ~535 MB for 500k lines (~10.7 kB/chunk at 1024 dims) |
| Search latency | 15 – 60 ms |

The break-even point is reached within a handful of sessions: a single
avoided exploration pays back several minutes of indexing.

---

## License

MIT OR Apache-2.0
