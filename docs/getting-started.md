# Getting started

From nothing to searching your own code. Roughly ten minutes of reading, plus
however long the first index takes.

Replace `<your-project>` throughout with the path you actually want indexed.
Nothing here assumes where it lives — any directory, on any disk, works.

---

## What you are setting up

Three pieces, and only the first two need to be running:

```
 ┌─ Docker ─────────────┐
 │  the embedding model │  loaded once, stays warm, shared by everything
 └──────────┬───────────┘
            │ HTTP
 ┌──────────▼───────────┐
 │  finderyd            │  the daemon: catalog, vectors, search
 └──────────┬───────────┘
            │ HTTP 127.0.0.1:8077
    ┌───────┴────────┬──────────────────┐
 findery          findery-mcp        your Rust code
 (the CLI)        (Claude Code)      (findery-client)
```

### Why a daemon at all?

A fair question — the CLI could open the SQLite catalog directly and skip it.

It exists for **one** reason that matters: the vector matrix. Searching means
comparing your query against every chunk, and that matrix is built in RAM. For a
500k-line codebase it is a few hundred megabytes. A daemon builds it once and
answers in ~15 ms; a process that starts fresh on every command would spend one
to three **seconds** reloading it before it could answer anything.

Two smaller reasons follow: `findery watch` needs a process that outlives your
shell, and SQLite has a single writer — a daemon serializes concurrent clients
instead of making them fight.

What the daemon is *not*: it is not the model. The model lives in Docker and is
the heavy part. The daemon is a 5 MB Rust binary.

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

## 4. Start the daemon

```bash
finderyd
```

Keep it running — its own terminal, a background service, whatever you prefer.
Check it from anywhere:

```bash
findery health
# ready · model Qwen/Qwen3-Embedding-0.6B (1024 dims) · store sqlite · 0 project(s)
```

If it says `DEGRADED`, the model is not answering: go back to step 3.

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
| daemon does not answer | start `finderyd`; the error message prints the commands |
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
