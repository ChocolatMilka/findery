# Getting started

From nothing to searching your own code. Roughly ten minutes of reading, plus
however long the first index takes.

Replace `<your-project>` throughout with the path you actually want indexed.
Nothing here assumes where it lives — any directory, on any disk, works.

---

## What you are setting up

Two processes with two different jobs, and a set of thin clients.

```text
 ┌─ a model runtime ────────────┐
 │  the embedding model         │  text in, vector out. STATELESS.
 │  (llama.cpp, TEI, LM Studio) │  It does not know what a project,
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

**There is only one server, and the model runtime is not it.** The runtime is
third-party inference software that turns text into vectors and forgets it ever
happened. Everything that knows about your code lives in `finderyd`.

`finderyd` itself can run either way — natively (the default) or in a container
of its own (`make up`). Same binary, same behaviour.

A containerized daemon cannot read your disk, so the CLI reads the files and
pushes them instead.

That choice is made once, at registration, by testing the path; you never have
to think about it. It also means the daemon can live on another machine
entirely.

### Why a daemon at all?

A fair question — the CLI could open the SQLite bucket directly and skip it.

It exists for **one** reason that matters: the warm matrix. Searching means
comparing your query against every chunk, from a matrix of normalized vectors
held contiguously in RAM. For a 500k-line codebase that is a few hundred
megabytes.

A daemon builds it once and answers in ~15 ms; a process starting fresh on every
command would spend one to three **seconds** loading it back from SQLite before
it could compare anything.

Note what that is *not*: it is not the vector cache. Two different things are
easy to conflate.

| | Lives in | Holds | Survives a restart |
|---|---|---|---|
| **Vector cache** | the `embeddings` table on disk | vectors keyed by `(recipe, content_hash)` — what makes a re-sync free | yes |
| **Warm matrix** | the daemon's RAM | the same vectors, contiguous and normalized, for scanning | no, rebuilt at boot |

The cache means you never re-embed. The daemon means you never wait. The cache
would still be there with no daemon at all; you would just pay seconds per query.

Two smaller reasons follow:

- `findery watch` needs a process that outlives your shell.
- SQLite has a single writer — a daemon serializes concurrent clients instead of
  letting them fight.

---

## 1. Prerequisites

- **Rust** — stable. `rustc --version` should answer.
- **A model runtime** — none to install by hand: step 3 sets one up natively,
  or in Docker if you prefer.
- **Windows only** — Git for Windows. It is what supplies the `bash` the two
  repository watchdogs are written for; `.\tools\tasks.ps1` looks for it there
  and not in WSL.

---

## 2. Install the binaries

```bash
git clone <this-repo> && cd findery
make install
```

That puts three commands in `~/.cargo/bin`:

| Command       | Role                            |
|---------------|---------------------------------|
| `findery`     | the CLI you type every day      |
| `finderyd`    | the daemon                      |
| `findery-mcp` | the bridge Claude Code launches |

No `make` — Windows, for instance? The same task list ships as PowerShell:

```powershell
.\tools\tasks.ps1 install
.\tools\tasks.ps1 help       # every task, same names as the Makefile
```

Or the three raw lines: `cargo install --path crates/cli --force`, and the same
for `crates/server` and `crates/mcp`.

**Windows without a Rust toolchain**: every release publishes an installer.

```powershell
msiexec /i findery-0.5.0-x64.msi      # the .msi from the latest release
findery --version                     # on PATH, from a new terminal
```

It puts the three binaries in `C:\Program Files\findery` and that directory on
the system `PATH` — all three together, because `findery` looks for `finderyd`
beside itself rather than on `PATH`, so the pair can never come from two
builds.

The index itself keeps living wherever your projects do; the installer places
the *tools*, not your data.

The build behind it is [`packaging/windows/`](../packaging/windows/README.md).

---

## 3. Start a model

findery talks to any `/v1/embeddings` endpoint. Pick **one** of the three ways
below, then point findery at it — two settings, written to the file the daemon
reads, so they survive the terminal and the reboot.

### A — Natively, with the bundled runtime

[`runtime/`](../runtime/) installs llama.cpp behind llama-swap: one port,
`127.0.0.1:8090`, serving Harrier and Qwen3-Embedding (Q8_0), routed by the
name in each request. Both binaries are pinned in
[`runtime/pins.txt`](../runtime/pins.txt) and every download is checked
against its sha256. The default build uses Vulkan, which runs on any recent
AMD, NVIDIA or Intel GPU; `cpu` is there for a machine without one.

| | Windows | Linux |
|---|---|---|
| Install | `.\runtime\models.ps1 install` | `runtime/models.sh install` |
| Without a GPU | `.\runtime\models.ps1 install -Backend cpu` | `runtime/models.sh install --backend cpu` |
| Start, in the background | `.\runtime\models.ps1 start` | `runtime/models.sh start` |
| Start at every login | `.\runtime\models.ps1 autostart on` | `runtime/models.sh autostart on` *(a systemd user service)* |
| Same, through the task runner | `.\tools\tasks.ps1 models-install`, `models-start` | `make models-install`, `make models-start` |

Then:

```bash
findery config set url   http://127.0.0.1:8090
findery config set model harrier-oss-v1-0.6b
```

Everything lands in a runtime directory beside findery's data —
`%LOCALAPPDATA%\findery\runtime` on Windows,
`~/.local/share/findery/runtime` on Linux (`FINDERY_RUNTIME_DIR` moves it).
The models are read from your LM Studio library (`~/.lmstudio/models`) when
it has them and downloaded otherwise, about 1.3 GB for the two; `--models
<dir>` puts them elsewhere. The served names are the ones LM Studio uses, so
vectors computed through LM Studio are reused.

To watch it:

| Command | Answers |
|---|---|
| `status` | whether it answers, which models are loaded, which files are missing |
| `ui` | opens the llama-swap dashboard, `http://127.0.0.1:8090/ui` |
| `logs` | follows the log (`journalctl` when it runs as a service) |
| `devices` | the GPUs llama.cpp sees — add `--device Vulkan1` in `llama-swap.yaml` to pick another |
| `stop`, `restart`, `uninstall` | what they say; `uninstall` keeps the config and the models |

`install` can be run again at any time: a step already done is skipped, and
the `llama-swap.yaml` it wrote is never overwritten, so edit it freely. A model
unloads after 30 idle minutes and loads again on the next request.

On Linux the GPU build needs the Vulkan loader (`libvulkan1` and your GPU's
Vulkan driver, `mesa-vulkan-drivers` on AMD and Intel); `devices` says whether
it sees one.

### B — In Docker

Hugging Face TEI serving Harrier, on CPU (`make up-model-gpu` for CUDA):

```bash
make up-model                # docker compose up -d tei
docker compose logs -f tei   # wait for it to report ready
findery config set url   http://127.0.0.1:8080
findery config set model microsoft/harrier-oss-v1-0.6b
```

`docker compose` is identical on every platform, so `.\tools\tasks.ps1 up-model`
and the raw command are interchangeable here.

The first run downloads about 1.2 GB of weights. They land in a Docker volume
and survive `docker compose down`, so this happens once.

### C — A runtime you already run

Anything serving `/v1/embeddings` works — LM Studio, Ollama, vLLM, or a hosted
provider:

```bash
findery config set url   http://127.0.0.1:1234      # LM Studio; Ollama is :11434
findery config set model <the name that runtime reports>
```

Left untouched, the configuration points at DashScope's hosted
`qwen3.7-text-embedding`, which needs a key — see
[step 4bis](#4bis-settings) for the whole surface.

---

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
a service manager or for debugging.

`FINDERY_NO_SPAWN=1` forbids the automatic start, for a container that owns the
process itself.

An index whose model is down still starts, and says so: `findery doctor` reports
`FAIL model` with the fix, and only the commands that need a vector fail. The
diagnostic is the last thing that should go down with the model.

---

## 4bis. Settings

`findery config` writes the file the daemon reads. Nothing goes in your shell,
so a setting survives the terminal that made it.

```bash
# DashScope's hosted qwen3.7-text-embedding, the built-in default
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
shell. The order is:

1. `--server` / `--token`
2. `FINDERY_SERVER_URL` / `FINDERY_API_TOKEN`
3. this file
4. loopback

So a machine configured once stays configured across terminals and reboots.

Three things it does that a text editor does not:

- **It refuses a value the daemon would reject**, before writing. A typo is
  answered with the list of real field names; `batch_size 0` is answered with
  the rule it breaks. You cannot write a file that stops the daemon booting.
- **It restarts the daemon**, because the daemon read its settings at boot and
  would otherwise keep serving the old ones.
- **It warns when an environment variable masks what you just wrote.** The
  `FINDERY_*` overlay still wins — that is what lets one container image serve
  every deployment — so it names the variable instead of letting the write look
  like it did nothing.

---

## 5. Index a project

```bash
cd <your-project>
findery init          # registers the directory, writes .findery/config.toml
findery sync          # indexes it
```

`init` leaves a manifest at `.findery/config.toml` and the index beside it:

- **The manifest** — yours to edit and to commit.
- **The index** — gitignored for you, in a `.findery/.gitignore` that `init`
  writes once.

The project id is derived from the directory name, and the daemon reads your
real filesystem — no mount to declare, no path to type.

The first pass takes minutes on a large repository, so `sync` draws two bars
while it runs — files, then embeddings, each once it means something:

```text
files ============================ 312/312 · 6 888 unchanged
embed ===============>             1 541/2 300 · 5 937 cached · 38/s · ETA 0:42
```

The counts are not guesses: the pass is planned before anything is read, so the
bar knows how many files are left to read and how many are unchanged.

Piped or redirected, there is no bar — just the summary line a script can read.

> **Registration and indexing are two steps on purpose.** A repository can be
> claimed, and its bucket chosen, before any model is reachable — so the setup
> does not fail because the embedding runtime is not up yet.

`findery init --sync` does both in one go when you know the model is there.

By default it indexes every language the chunkers know —
`rs, md, toml, js, mjs, cjs, ts, py, php, json, axle, java, kt, kts, go`. To
choose:

```bash
findery init --ext rs,md,toml,yml,sql
```

### More than one index

Each project gets its own **bucket** by default: its chunks and vectors, in
SQLite files under `.findery/` inside the project — one file per model, named
after it. Nothing to create, nothing to name, and the index travels with the
code.

A bucket becomes shared when two projects point at the same path. Declare that
in the manifest:

```toml
[bucket]
name = "team"
path = "D:/shared/team.db"
```

```bash
findery bucket ls                    # what each holds, file by file
findery bucket create work           # a named one, for `findery init --bucket work`
```

The choice is made once, at registration. Every later command follows the
project, so nobody has to remember which index a directory belongs to.

`findery bucket rm <name>` removes one along with its projects and vectors; it
refuses while the bucket still holds projects unless you add `--force`.

### Restricting a search without a second index

The same manifest declares **scopes** — named restrictions a search asks for by
name. They filter the index you already have, before scoring: no second index,
no re-indexing, no cut applied after the ranking.

```toml
[scopes.api]
paths = ["crates/server/**", "crates/client/**"]
langs = ["rust"]
```

```bash
findery search --scope api "where is the retry policy"
```

---

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

---

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

Restart Claude Code and type `/mcp`. You should see five tools — `search`,
`read`, `projects`, `sync`, `note` — and seven prompts. `search` becomes the
agent's first move when locating anything, instead of a glob, a handful of greps
and three file reads.

Claude Code launches the bridge, not your shell, so it takes its daemon address
from the `env` block of this file rather than from your settings. Add it when
the daemon is not on loopback:

```json
      "env": { "FINDERY_SERVER_URL": "https://findery.internal.example.com" }
```

If `findery-mcp` is not on the PATH Claude Code sees, use its absolute path.

---

## What to expect

The vector scan and the disk figures are measured (`make bench`, and a real
bucket at ~10.7 kB per chunk); the first-index times are estimates until the
end-to-end benchmark exists ([roadmap R8](../.claude/roadmap/README.md)). They
are for CPU; a GPU is five to ten times faster.

| Codebase             | Files  | Chunks  | First index (CPU) | Disk    | Vector scan |
|----------------------|--------|---------|-------------------|---------|-------------|
| Small (~10k lines)   | ~70    | ~1,000  | 15-30 s           | ~11 MB  | < 1 ms      |
| Medium (~100k lines) | ~500   | ~10,000 | 2-5 min           | ~110 MB | ~2.5 ms     |
| Large (~500k lines)  | ~3,000 | ~50,000 | 10-25 min         | ~535 MB | ~12 ms      |

A search adds the query's own embedding to the scan: 15 to 60 ms end to end.

The first index is the only slow moment. Everything after is incremental, and
re-indexing an unchanged repository costs **zero** embeddings — vectors are keyed
by content, so re-cloning the repo or running a `--full` is free.

Moving a function to another file is the exception: the path is part of the text
that was embedded, deliberately, so the fragments that moved are embedded again.

Two numbers worth reading in the sync report:

- `warnings` — files skipped for being too large or not UTF-8
- `embeddings_computed` versus `embeddings_cached` — the second run should
  compute nothing

---

## If something is off

| Symptom | What to do |
|---------|-----------|
| `no indexed project contains …` | `findery init` here, or `-p <id>` |
| daemon does not answer | `findery doctor` — it names the cause and the fix |
| daemon will not start | `<data-dir>/finderyd.log` has the reason; `findery doctor` prints the path |
| searches print `keyword results only: …` | the model is down: `findery doctor`, then `runtime/models.* status` or `docker compose logs tei` |
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

- **Every flag and knob** — [`.claude/docs/usage/README.md`](../.claude/docs/usage/README.md)
- **Choosing a model**, local or remote — [`.claude/docs/embed/README.md`](../.claude/docs/embed/README.md)
- **Choosing a vector store**, with the measurements — [`.claude/docs/store/README.md`](../.claude/docs/store/README.md)
- **HTTP API reference** — [`.claude/docs/server/README.md`](../.claude/docs/server/README.md)
- **How it works inside** — [`.claude/docs/overview/pipeline.md`](../.claude/docs/overview/pipeline.md)
