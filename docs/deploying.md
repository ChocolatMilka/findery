# Shipping findery to someone else

This page is for the person who has to hand findery to a team and never hear
about it again. It assumes nothing about the reader except that they can type a
command.

The short version: **ship three binaries and a model endpoint.** Everything
else starts itself.

---

## What actually has to exist on a user's machine

| Thing | Who starts it | Survives a reboot |
|---|---|---|
| `findery` | the user, by typing it | n/a, it exits |
| `finderyd` (the daemon) | **itself**, on the first command | no — the next command starts it again |
| `findery-mcp` | Claude Code | n/a |
| an embedding endpoint | you decide — see below | depends on your choice |

That is the entire surface. There is no service to register, no port to open,
no database to provision. The daemon listens on `127.0.0.1:8077` and writes one
SQLite file.

### Where the data lives

| Platform | Path |
|---|---|
| Windows | `%LOCALAPPDATA%\findery` |
| macOS / Linux | `$XDG_DATA_HOME/findery`, else `~/.local/share/findery` |

`FINDERY_DATA_DIR` moves it. The directory holds `catalog.db` (everything: files,
chunks, vectors, the cache) and `finderyd.log`. Deleting it is a full reset and
costs one re-index.

---

## The one decision you have to make

Where do vectors come from? Everything else follows.

### A. Each user runs a local model

The most private option and the one with no running cost. The user installs
LM Studio, Ollama or llama.cpp, loads an embedding model, and you set two
variables.

```bash
export FINDERY_EMBED_BASE_URL=http://127.0.0.1:1234   # LM Studio
export FINDERY_EMBED_MODEL=Qwen3-Embedding-0.6B-GGUF
```

Cost: nothing. Support burden: you now depend on each user keeping a runtime
alive. `findery doctor` makes that failure self-explanatory, which is most of
what you need.

### B. One shared embedding endpoint

A single GPU box, or a hosted provider. Every user points at it.

```bash
export FINDERY_EMBED_BASE_URL=https://embed.internal.example.com
export FINDERY_EMBED_MODEL=Qwen/Qwen3-Embedding-0.6B
export FINDERY_EMBED_API_KEY=...
```

Each user still keeps their own index; only the embedding call is shared. This
is the usual answer for a team: one thing to operate, and it is stateless.

### C. One shared daemon

Everyone points `FINDERY_SERVER_URL` at a central `finderyd`. One index for the
whole team, one machine to operate.

```bash
export FINDERY_SERVER_URL=https://findery.internal.example.com
export FINDERY_API_TOKEN=...
```

The daemon on another machine cannot read anyone's disk, so clients walk and
push their own files — which is exactly how findery already works whenever the
daemon cannot see a root. Nothing to configure for it.

Set `server.api_token`. A daemon anyone on the network can query is a daemon
anyone on the network can read your source through.

**Clients never start a remote daemon.** If it is down, they say so and stop.
Starting a local one would answer from an empty index and look like the server
had lost everything.

---

## Installing

```bash
cargo install --path crates/cli --force
cargo install --path crates/server --force
cargo install --path crates/mcp --force
```

Or ship the three binaries in one directory. `findery` looks for `finderyd`
**next to itself first**, then on the `PATH` — so an unzipped folder works with
no installation at all, and a stale `finderyd` elsewhere on the `PATH` cannot
shadow the one you shipped.

### What to put in the environment

The only two that are not optional:

```bash
FINDERY_EMBED_BASE_URL=...   # where the model answers
FINDERY_EMBED_MODEL=...      # its name, as that runtime reports it
```

Everything else has a working default. Put them in the user's shell profile, in
an MDM-pushed environment, or in `/etc/findery/config.toml` — the daemon reads
that file if it exists, and environment variables override it.

---

## The user's first five minutes

```bash
cd their-project
findery init
# (started the daemon in the background)
# indexing `their-project` at /home/x/their-project
# incremental in 21 s — scanned 78 · +78 · chunks +1165 · 1165 embeddings computed
#
# ready — try: findery search "<your question>"

findery search "where do we validate an invoice"
```

Two commands, and one of them is a search. That is the whole onboarding.

### Claude Code

`.mcp.json` at the root of any project the agent should search:

```json
{
  "mcpServers": {
    "findery": {
      "command": "findery-mcp"
    }
  }
}
```

The bridge starts the daemon too, so a user who only ever uses Claude Code never
types a findery command at all. Add
`"env": { "FINDERY_SERVER_URL": "https://findery.internal.example.com" }` for
the shared-daemon setup.

---

## When it goes wrong

One command, and it is the only one you should have to teach:

```console
$ findery doctor
OK   daemon       answering at http://127.0.0.1:8077
FAIL model        Qwen/Qwen3-Embedding-0.6B (1024 dims) · store sqlite — connection refused
       → start your embedding runtime (LM Studio, Ollama, `make up-model`),
       → then check FINDERY_EMBED_BASE_URL and FINDERY_EMBED_MODEL
OK   here         `their-project` covers this directory (3 project(s) indexed)
OK   storage      /home/x/.local/share/findery/catalog.db
OK   daemon log   /home/x/.local/share/findery/finderyd.log

1 problem(s) above, each with its fix.
```

Every line that fails carries its own remedy. `findery --json doctor` gives the
same thing to a support script.

Note what the example shows: **the model is down and the daemon is up.** That is
deliberate. A daemon that refused to boot without its model would take the
diagnostic down with it at the exact moment it is needed. Instead it starts,
reports the model as unreachable, and fails only the commands that need a
vector.

| What the user says | What it is |
|---|---|
| "it says no indexed project contains …" | they are outside every indexed root — `findery init`, or `-p <id>` |
| "searches return nothing" | not synced, or a filter too narrow — `findery status` |
| "it re-embedded everything" | the model or its dimension changed; that is a new cache namespace, and it is correct |
| "the first index is taking forever" | it is the one slow step — `findery jobs` shows progress |
| "nothing happens at all" | `findery doctor`, then the log path it prints |

---

## Running the daemon as a service

You do not have to. Auto-start covers the laptop case completely, and a daemon
that dies is replaced by the next command the user types.

Do it when the daemon is shared (option C), or when you want it up before anyone
logs in.

```ini
# /etc/systemd/system/finderyd.service
[Unit]
Description=findery index daemon
After=network.target

[Service]
ExecStart=/usr/local/bin/finderyd
Environment=FINDERY_DATA_DIR=/var/lib/findery
Environment=FINDERY_EMBED_BASE_URL=http://127.0.0.1:8080
Environment=FINDERY_BIND=0.0.0.0:8077
Environment=FINDERY_API_TOKEN=change-me
Restart=on-failure
User=findery

[Install]
WantedBy=multi-user.target
```

Set `FINDERY_NO_SPAWN=1` in the users' environment so clients report a stopped
service instead of quietly starting a second one beside it.

`docker compose up -d` is the same arrangement in a container; see
`docker-compose.yml`.

---

## Upgrading

Replace the binaries and run `findery daemon stop`. The next command starts the
new one.

The index survives an upgrade. It does **not** survive changing the embedding
model or its dimension — the catalog records the recipe it was built with and
refuses to mix two vector spaces, which is the correct answer but means a
re-index. Plan model changes the way you would plan a schema migration.

---

## What you are not shipping

Worth saying plainly, because it is what people ask:

- **No telemetry.** Nothing leaves the machine except the embedding call.
- **No network exposure by default.** `127.0.0.1` only.
- **No source code to a third party**, unless you chose a hosted embedding
  provider — in which case chunk text goes to it, and that is worth telling your
  users.
- **No background CPU.** The daemon is idle until asked. `findery watch` is the
  exception and the user starts it on purpose.
