# `findery-mcp` — wiring the index into Claude Code

The bridge is **stateless**: every call becomes an HTTP request to the
daemon. That's what lets several editors, several projects, and several
machines share a single warm model. The bridge is a wire, not a cache
(CONV-008).

## Wiring it in

### The native binary (recommended)

```bash
cargo build --release -p findery-mcp   # → target/release/findery-mcp
```

`.mcp.json` at the root of the project to index, or `~/.claude.json` for
all of them:

```json
{
  "mcpServers": {
    "findery": {
      "command": "C:/Program Files/findery/findery-mcp.exe"
    }
  }
}
```

Three spellings of the same path, depending on how it was installed: the one
above after the Windows `.msi`, `<repo>/target/release/findery-mcp` from a source
checkout, and whatever `~/.cargo/bin` resolves to after `make install`. Name the
bridge from the **same build as the server it will talk to** — the bridge is a
wire, and a wire to a different version is a wire to a different API.

Add an `env` block only when the daemon is not on loopback:

```json
      "env": {
        "FINDERY_SERVER_URL": "https://findery.internal.example.com",
        "FINDERY_API_TOKEN": "<the bearer token it asks for>"
      }
```

INTENT: Claude Code launches this process, not a shell, so the `env` block of
`.mcp.json` is the bridge's configuration file — it is where an MCP server is
configured, not a workaround for a missing one. The default it falls back to is
`findery_core::config::DEFAULT_SERVER_URL`, the same constant the CLI reads out
of the settings file, so an unconfigured pair still meets on loopback. Note the
asymmetry: `findery config set server …` names the daemon for the **CLI**, not
for this bridge. Point both when the daemon is remote.

### Or through Docker, with nothing to install

```json
{
  "mcpServers": {
    "findery": {
      "command": "docker",
      "args": ["run", "--rm", "-i", "--network", "findery_default",
               "-e", "FINDERY_SERVER_URL=http://findery:8077",
               "--entrypoint", "/usr/local/bin/findery-mcp",
               "findery:latest"]
    }
  }
}
```

Then `claude mcp list` to check, `/mcp` in the session to see the tools.

**stdout belongs to the protocol.** Logs go to stderr (`FINDERY_LOG`). A stray
`println!` corrupts the JSON-RPC stream and the client kills the session.

## The tools

| Tool | What it does |
|-------|---------------|
| `search_code` | hybrid search; **the main tool** |
| `list_projects` | which projects are indexed, and under what id |
| `index_status` | files, chunks, vectors, model, last sync |
| `sync_project` | re-sync (incremental, full, or specific paths) |
| `job_status` | where a running sync is: files done, chunks, embeddings |
| `register_project` | index a new directory |
| `add_document` | add a document, including plain text with no file |
| `remove_document` | remove a document from the index |
| `export_vectors` | export vectors (jsonl / json / binary) |

### A cold index is minutes, so it is a job

`sync_project` on an index that has never been built takes minutes, and an MCP
call that blocks that long is a call that times out and gets retried. That is
why it does not wait by default: the tool returns a job id immediately, and
`job_status` answers with what that job has done so far —

```
job `a3f1…` — axle · full · running
  indexing · 412/2048 files · 3180 chunks · 900 embedded, 2280 cached · 4.1s
```

`files_total` is the walk's own count, so the fraction is real rather than an
estimate. Once the job is over, `job_status` prints its `SyncReport` instead:
there is only ever one description of a finished job.

### The rendering is the product

A tool that returns raw JSON spends a third of its tokens on punctuation
and field names the model has no use for. Every response is laid out to be
read once and cited:

```text
3 excerpt(s) in `axle` for "where is retry backoff handled"

[1] crates/net/src/retry.rs:12-48 · method `Client > retry_with_backoff` · 0.842
C:/Users/users/RustroverProjects/axle/crates/net/src/retry.rs
```rust
pub fn retry_with_backoff(&self, attempts: u32) -> Result<()> {
    …
}
```
```

Three things per hit, and nothing else: a **locator** `path:12-48` the
agent can cite, a **host path** it can open, and **the excerpt**.

An empty search doesn't just say "nothing": it says what to try next
(`mode: "keyword"` for an exact identifier, `sync_project` if the code is
newer than the index).

## Token budget

| Lever | Effect |
|--------|--------|
| `task` | retrieval intent: `code` `symbol` `usage` `docs` `test` `config` `error` — picking the right one measurably improves ranking |
| `instruction` | free-form instruction, overriding `task` |
| `k` | number of excerpts (default 8) |
| `max_snippet_chars` | cap **per excerpt** — the most direct lever |
| `paths`, `langs`, `kinds` | narrow before scoring, not after |
| `mode: "keyword"` | for an exact identifier, no semantics to pay for |

Order of magnitude: `k=8` with 1200-character excerpts ≈ **2,500 to 3,500
tokens** for a full response. The same question resolved through
glob + grep + three `Read`s commonly costs 25,000 to 60,000.

## Errors

A tool that fails returns a **normal result** with `isError: true`, not a
transport error: the model needs to see what broke and adapt, not receive a
JSON-RPC code it can't reason about.

A malformed frame returns `-32700` and **does not kill the session**.

## Protocol

JSON-RPC 2.0, newline-delimited. `initialize`,
`notifications/initialized`, `ping`, `tools/list`, `tools/call`. The
version the client announces is echoed back as-is when provided
(`2025-06-18` by default), which keeps older clients working.

Hand-written, ~80 lines: the surface actually needed is small, and writing
it ourselves pins the wire format to something this repo controls rather
than to a client library that revises its API faster than the spec.
