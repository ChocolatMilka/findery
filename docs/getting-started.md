# Getting started

From nothing to searching your own code. Roughly ten minutes of reading, plus
however long the first index takes.

Replace `<your-project>` throughout with the path you actually want indexed.
Nothing here assumes where it lives — any directory, on any disk, works.

---

## What you are setting up

Two processes with two different jobs, and a set of thin clients.

```
 ┌─ a container ────────────────┐
 │  the embedding model         │  text in, vector out. STATELESS.
 │  (HuggingFace TEI, Qwen3)    │  It does not know what a project,
 └──────────────┬───────────────┘  a chunk or your index is.
                │ HTTP — "embed these 32 texts"
 ┌──────────────▼───────────────┐
 │  finderyd                    │  THE server. It owns everything:
 │  catalog · vectors · search  │  the SQLite file, the warm matrix,
 │  jobs · watchers             │  search, jobs, watchers.
 └──────────────┬───────────────┘
                │ HTTP 127.0.0.1:8077
      ┌─────────┴────────┬──────────────────┐
   findery          findery-mcp        your Rust code
   (the CLI)        (Claude Code)      (findery-client)   ← no state
```

**There is only one server, and the container is not it.** The container runs a
third-party inference runtime that turns text into vectors and forgets it ever
happened. Everything that knows about your code lives in `finderyd`.

`finderyd` itself can run either way — natively (the default) or in a container
of its own (`make up`). Same binary, same behaviour.

A containerized daemon cannot read your disk, so the CLI reads the files and
pushes them instead. That choice is made once, at registration, by testing the
path; you never have to think about it. It also means the daemon can live on
another machine entirely.

### Why a daemon at all?

A fair question — the CLI could open the SQLite catalog directly and skip it.

It exists for **one** reason that matters: the warm matrix. Searching means
comparing your query against every chunk, from a matrix of normalized vectors
held contiguously in RAM. For a 500k-line codebase that is a few hundred
megabytes. A daemon builds it once and answers in ~15 ms; a process starting
fresh on every command would spend one to three **seconds** loading it back from
SQLite before it could compare anything.

Note what that is *not*: it is not the vector cache. Two different things are
easy to conflate.

| | Lives in | Holds | Survives a restart |
|---|---|---|---|
| **Vector cache** | the `embeddings` table on disk | vectors keyed by `(recipe, content_hash)` — what makes a re-sync free | yes |
| **Warm matrix** | the daemon's RAM | the same vectors, contiguous and normalized, for scanning | no, rebuilt at boot |

The cache means you never re-embed. The daemon means you never wait. The cache
would still be there with no daemon at all; you would just pay seconds per query.

Two smaller reasons follow: `findery watch` needs a process that outlives your
shell, and SQLite has a single writer — a daemon serializes concurrent clients
instead of letting them fight.

---

## 1. Prerequisites

- **Rust** (stable). `rustc --version` should answer.
- **Docker**, running. Only to host the model — see step 2 for the alternative.

## 2. Install the binaries

```bash
git clone <this-repo> && cd findery
make install
```

That puts three commands in `~/.cargo/bin`:

| Command | Role |
|---------|------|
| `findery` | the CLI you type every day |
| `finderyd` | the daemon |
| `findery-mcp` | the bridge Claude Code launches |

No `make`? `cargo install --path crates/cli --force` and the same for
`crates/server` and `crates/mcp`.

## 3. Start the model

```bash
make up-model              # docker compose up -d tei
docker compose logs -f tei # wait for it to report ready
```

First run downloads about 1.2 GB of weights. They land in a Docker volume and
survive `docker compose down`, so this happens once.

**Already running a model?** Skip this entirely. Anything serving
`/v1/embeddings` works — Ollama, llama.cpp, LM Studio, vLLM, or a hosted
provider. Point the daemon at it and move on:

```bash
export FINDERY_EMBED_BASE_URL=http://127.0.0.1:11434
export FINDERY_EMBED_MODEL=qwen3-embedding:0.6b
```

## 4. The daemon — there is nothing to do

There is no command here. The first `findery` you type notices that nothing is
answering and starts `finderyd` in the background:

```
(started the daemon in the background)
```

No terminal to keep open, no service to install. It writes to
`<data-dir>/finderyd.log` and stays up until the machine restarts.

Only a **local** daemon starts itself. If `FINDERY_SERVER_URL` points at another
machine, a failure to reach it is reported, never replaced by a local one
answering from an empty index.

```bash
findery doctor        # what is up, what is not, and the command for each
findery daemon stop   # when you want it gone
findery daemon start  # when you want it back
```

`FINDERY_NO_SPAWN=1` forbids the automatic start, for a service manager or a
container that owns the process itself.

A daemon whose model is down still starts, and says so: `findery doctor` reports
`FAIL model` with the fix, and only the commands that need a vector fail. The
diagnostic is the last thing that should go down with the model.

## 5. Index a project

```bash
cd <your-project>
findery init
```

That is the whole setup. The project id is derived from the directory name, and
the daemon reads your real filesystem — no mount to declare, no path to type.

By default it indexes `.rs` and `.md`. To widen:

```bash
findery init --ext rs,md,toml,yml,sql
```

## 6. Search

From anywhere inside the project — the one containing your current directory is
selected automatically:

```bash
findery search "where is retrying handled"
findery search "ChunkId" --task symbol       # you have a name, you want the definition
findery search "E0432" --mode keyword        # exact identifier, skip the semantics
findery search "retry" --path 'crates/**' --lang rust
```

Each hit gives you a `path:start-end` locator you can quote, an absolute path
you can open, and the excerpt itself.

Day to day:

```bash
findery sync        # after editing — near instant
findery watch       # or let it resync by itself
findery status      # files, chunks, vectors
findery projects    # all of them, current one marked *
```

## 7. Plug it into Claude Code

Create `.mcp.json` at the root of the project you want the agent to search:

```json
{
  "mcpServers": {
    "findery": {
      "command": "findery-mcp",
      "env": { "FINDERY_SERVER_URL": "http://127.0.0.1:8077" }
    }
  }
}
```

Restart Claude Code and type `/mcp`. You should see `search_code` and seven
other tools. `search_code` becomes the agent's first move when locating
anything — instead of a glob, four greps and three file reads.

If `findery-mcp` is not on the PATH Claude Code sees, use its absolute path.

---

## What to expect

Sizing from real measurements. Times are for CPU; a GPU is five to ten times
faster.

| Codebase | Files | Chunks | First index (CPU) | Disk | Search |
|----------|-------|--------|-------------------|------|--------|
| Small (~10k lines) | ~70 | ~1,000 | 20-40 s | ~11 MB | ~3 ms |
| Medium (~100k lines) | ~500 | ~12,000 | 4-8 min | ~130 MB | ~5 ms |
| Large (~500k lines) | ~3,000 | ~60,000 | 20-40 min | ~640 MB | ~15 ms |

The first index is the only slow moment. Everything after is incremental, and
re-indexing an unchanged repository costs **zero** embeddings — vectors are keyed
by content, so moving a function, renaming a module or re-cloning the repo is
free.

Two numbers worth reading in the sync report: `warnings` (files skipped for
being too large or not UTF-8) and `embeddings_computed` versus
`embeddings_cached` — the second run should compute nothing.

---

## If something is off

| Symptom | What to do |
|---------|-----------|
| `no indexed project contains …` | `findery init` here, or `-p <id>` |
| daemon does not answer | `findery doctor` — it names the cause and the fix |
| daemon will not start | `<data-dir>/finderyd.log` has the reason; `findery doctor` prints the path |
| `DEGRADED` / every search returns 502 | the model is down — `docker compose logs tei` |
| search finds nothing that clearly exists | not synced yet, or a filter is too narrow — check `findery status` |
| sync re-embeds everything | the model or dimension changed, which opens a new cache namespace |
| first index feels endless | it is the one slow step; `findery jobs` shows progress |

`FINDERY_LOG=debug finderyd` gives the full trace.

---

## Going further

- **Every flag and knob**: [`.claude/docs/usage/README.md`](../.claude/docs/usage/README.md)
- **Choosing a model**, local or remote: [`.claude/docs/embed/README.md`](../.claude/docs/embed/README.md)
- **Choosing a vector store**, with the measurements: [`.claude/docs/store/README.md`](../.claude/docs/store/README.md)
- **HTTP API reference**: [`.claude/docs/server/README.md`](../.claude/docs/server/README.md)
- **How it works inside**: [`.claude/docs/overview/pipeline.md`](../.claude/docs/overview/pipeline.md)
