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
 │  bucket · vectors · search  │  the SQLite file, the warm matrix,
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

A fair question — the CLI could open the SQLite bucket directly and skip it.

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
- **Windows only**: Git for Windows. It is what supplies the `bash` the two
  repository watchdogs are written for; `.\tools\tasks.ps1` looks for it there
  and not in WSL.

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

No `make` — Windows, for instance? The same task list ships as PowerShell:

```powershell
.\tools\tasks.ps1 install
.\tools\tasks.ps1 help       # every task, same names as the Makefile
```

Or the three raw lines: `cargo install --path crates/cli --force`, and the same
for `crates/server` and `crates/mcp`.

## 3. Start the model

```bash
make up-model              # docker compose up -d tei
docker compose logs -f tei # wait for it to report ready
```

`docker compose` is identical on every platform, so `.\tools\tasks.ps1 up-model`
and the raw command are interchangeable here.

First run downloads about 1.2 GB of weights. They land in a Docker volume and
survive `docker compose down`, so this happens once.

**Already running a model?** Skip this entirely. Anything serving
`/v1/embeddings` works — Ollama, llama.cpp, LM Studio, vLLM, or a hosted
provider. Point the daemon at it and move on:

```bash
findery config set url http://127.0.0.1:11434
findery config set model <the name that runtime reports>
```

These are written to a file the daemon reads, not to your shell: they survive
the terminal, the reboot, and the next person who opens a different one. See
[step 4bis](#4bis-settings) for the whole surface.

## 4. The background index — there is nothing to do

There is no command here, and no output either. The first `findery` you type in a
project notices that nothing is answering and brings the index up in the
background. It says nothing: a sentence about a process you never asked for is a
sentence between you and your answer.

No terminal to keep open, no service to install. It writes to
`<data-dir>/finderyd.log` and stays up until the machine restarts.

Only a **local** index starts itself. If the address points at another machine, a
failure to reach it is reported, never replaced by a local one answering from an
empty index.

```bash
findery doctor        # what is up, what is not, and the command for each
```

`findery daemon start` / `stop` / `status` still exist, hidden from `--help`, for
a service manager or for debugging. `FINDERY_NO_SPAWN=1` forbids the automatic
start, for a container that owns the process itself.

An index whose model is down still starts, and says so: `findery doctor` reports
`FAIL model` with the fix, and only the commands that need a vector fail. The
diagnostic is the last thing that should go down with the model.

## 4bis. Settings

`findery config` writes the file the daemon reads. Nothing goes in your shell,
so a setting survives the terminal that made it.

```bash
findery config set model qwen3.7-text-embedding
findery config set url   https://dashscope-intl.aliyuncs.com/compatible-mode
findery config set key   <your API key>
findery config set dim   2560          # 256 … 2560; changing it is a re-index

findery config show                    # what is set, secrets masked
findery config unset dim               # back to the default
findery config path                    # the file itself
```

`model`, `url`, `key`, `dim`, `store`, `token`, `bind`, `instruction` and
`extensions` are short names; anything else takes its full path, such as
`search.rrf_k` or `embedder.query_instruction`.

`url` is the **embedding endpoint**. The daemon's own address is a different
setting with its own short name:

```bash
findery config set server https://findery.internal.example.com
findery config set token  <the bearer token it asks for>
```

That is how a client on this machine finds its daemon with nothing in the
shell. The order is `--server` / `--token`, then `FINDERY_SERVER_URL` /
`FINDERY_API_TOKEN`, then this file, then loopback — so a machine configured
once stays configured across terminals and reboots.

Three things it does that a text editor does not:

* **It refuses a value the daemon would reject**, before writing. A typo is
  answered with the list of real field names; `batch_size 0` is answered with
  the rule it breaks. You cannot write a file that stops the daemon booting.
* **It restarts the daemon**, because the daemon read its settings at boot and
  would otherwise keep serving the old ones.
* **It warns when an environment variable masks what you just wrote.** The
  `FINDERY_*` overlay still wins — that is what lets one container image serve
  every deployment — so it names the variable instead of letting the write look
  like it did nothing.

## 5. Index a project

```bash
cd <your-project>
findery init          # registers the directory
findery sync          # indexes it
```

The project id is derived from the directory name, and the daemon reads your
real filesystem — no mount to declare, no path to type.

The first pass takes minutes on a large repository, so `sync` draws a bar while
it runs:

```
⠸ =>===============------------- 892/3104 files · indexing · 6104 chunks · 1430 embedded / 4674 cached
```

The `3104` is a count of files, not a guess: the walk completes before anything
is read, which is what lets the bar track real progress. Piped or redirected,
there is no bar — just the summary line a script can read.

**Registration and indexing are two steps on purpose.** A repository can be
claimed, and its bucket chosen, before any model is reachable — so the setup
does not fail because the embedding runtime is not up yet. `findery init --sync`
does both in one go when you know the model is there.

By default it indexes `.rs` and `.md`. To widen:

```bash
findery init --ext rs,md,toml,yml,sql
```

### More than one index

A daemon serves several **buckets**: independent SQLite files, each holding its
own projects, chunks and vectors. Two repositories that should not share a file
— a work one and a personal one, say — go in two buckets, and a client can be
pointed at either.

```bash
findery bucket create work
findery init --bucket work
findery bucket ls                    # what each holds, and where it lives
```

The bucket is chosen once, at registration. Every later command follows the
project, so nobody has to remember which index a directory belongs to.
`findery bucket rm <name>` removes one along with its projects and vectors; it
refuses while the bucket still holds projects unless you add `--force`.

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
      "command": "findery-mcp"
    }
  }
}
```

Restart Claude Code and type `/mcp`. You should see `search_code` and seven
other tools. `search_code` becomes the agent's first move when locating
anything — instead of a glob, four greps and three file reads.

Claude Code launches the bridge, not your shell, so it takes its daemon address
from the `env` block of this file rather than from your settings. Add it when
the daemon is not on loopback:

```json
      "env": { "FINDERY_SERVER_URL": "https://findery.internal.example.com" }
```

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
| a sync is refused, naming two recipes | that bucket holds vectors from another model — `findery bucket ls` names the conflict, and keyword search still works there |
| first index feels endless | it is the one slow step; `findery jobs` shows progress |

### Changing a setting

Use `findery config set`; it writes the file and restarts the daemon for you.
See [step 4bis](#4bis-settings).

Only `FINDERY_LOG` still has to be an environment variable, because it is read
before any configuration exists:

```bash
findery daemon stop
export FINDERY_LOG=debug
findery status                    # starts a daemon that sees it
```

`finderyd --check` prints the configuration as it actually resolves, without
binding anything.

---

## Going further

- **Every flag and knob**: [`.claude/docs/usage/README.md`](../.claude/docs/usage/README.md)
- **Choosing a model**, local or remote: [`.claude/docs/embed/README.md`](../.claude/docs/embed/README.md)
- **Choosing a vector store**, with the measurements: [`.claude/docs/store/README.md`](../.claude/docs/store/README.md)
- **HTTP API reference**: [`.claude/docs/server/README.md`](../.claude/docs/server/README.md)
- **How it works inside**: [`.claude/docs/overview/pipeline.md`](../.claude/docs/overview/pipeline.md)
