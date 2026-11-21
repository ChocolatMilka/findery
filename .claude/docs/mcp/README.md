# `findery-mcp` — the index as an agent's eyes

The bridge owns no index state: every call becomes HTTP requests to the daemon.
That's what lets several editors, several projects, and several machines share
a single warm model. The bridge is a wire, not a cache (CONV-008).

What it does own is **where it stands** — the directory Claude Code started it
in — because that is what lets a call leave `project` out.

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

Three spellings of the same path, depending on how it was installed:

- **after the Windows `.msi`** — the one above
- **from a source checkout** — `<repo>/target/release/findery-mcp`
- **after `make install`** — whatever `~/.cargo/bin` resolves to

Name the bridge from the **same build as the server it will talk to** — the
bridge is a wire, and a wire to a different version is a wire to a different
API.

Add an `env` block only when the daemon is not on loopback:

```json
      "env": {
        "FINDERY_SERVER_URL": "https://findery.internal.example.com",
        "FINDERY_API_TOKEN": "<the bearer token it asks for>"
      }
```

INTENT: Claude Code launches this process, not a shell, so the `env` block of
`.mcp.json` is the bridge's configuration file — it is where an MCP server is
configured, not a workaround for a missing one.

The default it falls back to is `findery_core::config::DEFAULT_SERVER_URL`, the
same constant the CLI reads out of the settings file, so an unconfigured pair
still meets on loopback.

> **Note the asymmetry** — `findery config set server …` names the daemon for
> the **CLI**, not for this bridge. Point both when the daemon is remote.

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

A container has no view of the host's working directory, so auto-detection
falls back to the registered root containing it — which a container never is:
name `project` in every call, or mount the project and set `-w`.

Then:

- `claude mcp list` to check
- `/mcp` in the session to see the tools

> **Note** — **stdout belongs to the protocol.** Logs go to stderr
> (`FINDERY_LOG`). A stray `println!` corrupts the JSON-RPC stream and the
> client kills the session.

## `project` is optional everywhere

A call that names no project is resolved the way the CLI resolves one, by the
same code (`findery_client::resolve`):

1. the `project` argument, when given
2. the `.findery/config.toml` at or above the bridge's directory — walked up
   like `package.json`, so it holds offline and in a clone
3. the registered project whose root contains that directory, **the deepest
   one** — a project nested inside another is picked over its parent

Nothing matching is a tool error that lists every known project with its root
and says how to register this directory. `projects` marks the detected one
with `*`.

MCP `roots` (a client telling the server its workspace folders) are not read
yet — roadmap.

## The five tools

| Tool | What it does | Replaces |
|------|---------------|---|
| `search` | hybrid search; **the main tool**. `intent` (enum of the built-in library, each with its one-line summary in the schema, grouped *locate* / *workflow*), `instruction`, `raw_query`, `like`, `mode`, `paths`, `langs`, `kinds`, `symbol`, `scope`, `k`, `max_snippet_chars`, `detail` | `search_code` |
| `read` | the lines a locator names (`path:L12-L40`) or a chunk by `id`, with `context` lines around; with `glob`, the indexed files a glob names (path · lang · chunks) | — |
| `projects` | `list` (default): root, model, language mix, chunk count, freshness, scopes, default intent · `status`: counters, model health, running job, what makes answers narrower · `register` (merges) · `intents`: the project's library with each workflow's legs | `list_projects`, `index_status`, `register_project` |
| `sync` | incremental, `full`, or `paths`; waits up to `wait` seconds (default 20) and answers the report, or the live progress and the job id; `sync {job}` follows it | `sync_project`, `job_status` |
| `note` | `add` a document from text (no file needed; never pruned by a sync), or `remove` one | `add_document`, `remove_document` |

`export_vectors` is CLI only (`findery export`): an admin gesture, not an agent
loop.

### Strict inputs

Every schema is `additionalProperties: false`, and the bridge enforces what
the schema says (`args.rs`). Each of these is a tool error that **lists the
valid values** — before anything is asked of the daemon:

- an undeclared argument (`querry`)
- a value of the wrong type (`k: "eight"`)
- a name outside its list (`langs: ["cobol"]`, `kinds: ["class"]`,
  `mode: "semantic"`, `detail: "brief"`, an unknown `action`)

The lists come from `findery-core` (`Lang::ALL`, `ChunkKind::ALL`,
`SearchMode::ALL`, the intent library), never typed in the bridge.

An `intent` the built-in library does not know is passed to the daemon, which
resolves it against the project's own layer and refuses it, listing the known
ones, when it is not there either.

### Registering merges

`projects {action: "register", root}` sends what it knows: the root, and —
when the root has a manifest — the manifest's id, bucket, scopes, instruction,
intents and pin.

The daemon **merges** a registration (`server/README.md`): whatever the
request leaves out is kept, so registering a root again from the bridge never
wipes what an earlier `findery init` declared. A root already registered keeps
its id even when none is sent.

### A cold index is a job

`sync` sends `wait_ms` and the daemon waits for its own job (bounded by
`server.max_sync_wait_ms`). Quick syncs answer their report inline; a cold
index answers where it is —

```text
sync `axle` (full) still running after 20 s — job `a3f1…`
  indexing · 412/2048 files · 3180 chunks · 900 embedded, 2280 cached · 4.1s
Follow it with sync {job: "a3f1…"}; search keeps answering from the index meanwhile.
```

— and `sync {job, wait}` polls it up to `wait` seconds more. The progress line
is `Progress::summary()`, the daemon's own description.

## The rendering is the product

A tool that returns raw JSON spends a third of its tokens on punctuation and
field names the model has no use for. Every answer is laid out to be read once
and cited (`render/`):

````text
note: keyword results only: no vectors under harrier yet — run `findery sync -p axle`
3 hits · `axle` · intent usage · "Given an API, function or type of a Rust codebase, retrieve the…" · model harrier
root C:/Users/users/RustroverProjects/axle

[1] crates/net/src/retry.rs:L12-L48 · method Client > retry_with_backoff
```rust
pub fn retry_with_backoff(&self, attempts: u32) -> Result<()> {
    …
}
```
(cut — read crates/net/src/retry.rs:L12-L48 for the rest)
````

- `note:` lines first — the answer is narrower than asked, and why.
- One header: count, project, intent, the instruction the model got (clipped),
  the model profile (`resolved.model`).
- The **root once**; each hit carries only its locator `path:L12-L48` — the
  form `read` and `like` take back. A hit's host path is printed only when it
  is not that root plus the relative path (a mount, a mixed answer).
- The kind and symbol (`method Client > retry`), then the fenced excerpt with
  its language tag (`typescript` for a `.ts`, `axle`, `go`…). An excerpt the
  daemon cut ends with where to read the rest.
- **No score.** A fused RRF value means nothing on its own; the rank is what
  it decided.

A workflow intent answers, in this order:

- one `## <leg> — N hits` section per search it ran
- `## pairs — docs → code`
  (`- README.md:L3-L9 possibly drifted → src/net.rs:L12-L48 (0.41)`, or
  `no counterpart found`)
- `## near-duplicates — copies that may have diverged` (`- a ≈ b (0.91)`)

With `detail: "compact"` (the default) each leg shows its top 3 hits in full
and the rest on one line (`[4] path:L1-L9 · function f — <first line>`);
`detail: "full"` shows every excerpt.

An empty search says what to try next: broader words, `mode: "keyword"`,
another intent, `sync`, `read {glob}` to check the file is indexed at all.

`read` prints `src/net.rs:L10-L48 of 120 · method Client > retry · from disk`,
`open <host path>`, then the lines — `from the index` for a pushed or inline
document, rebuilt from its chunks.

## Prompts

`prompts/list` and `prompts/get` serve one **playbook** per task — in Claude
Code `/mcp__findery__review <target> <scope>`:

| Prompt | Starts from | `target` |
|---|---|---|
| `review` | intent `review` | required: the function, type or module |
| `refactor` | intent `refactor` | required: the symbol |
| `doc-sync` | intent `doc-sync` | optional: a feature or area |
| `divergence` | intent `divergence` | optional |
| `onboard` | intent `onboard` | optional: an area |
| `security` | intent `security` | optional: a component |
| `bug-trace` | intents `error`, `flow`, `test` | required: the symptom |

`scope` narrows every step: a name the project declares becomes `scope: "…"`,
anything with `/`, `*` or `?` becomes `paths: ["…"]`.

A body is the playbook (`prompts/playbooks.rs`, data): the `search` / `read`
calls in order with their intents, what to verify, how to report — and a
**language checklist generated from the project's actual mix**
(`GET /v1/projects/{id}/stats` → `langs`).

Each language of the project gets one line:

- up to three constructs from each intent the playbook draws on (the intent
  library's per-language hints, so the checklist names what the index was told
  to look for)
- for the code-changing playbooks, what a change breaks in that language

A review of a Rust + Axle project, for instance:

```text
## Language checklist — rust 50%, axle 33%, markdown 16%
- Rust: look at call sites, `impl Trait for` blocks, generic bounds, `#[test]` and `#[tokio::test]`
  functions, `#[cfg(test)]` modules, `tests/` files, `?` propagation, … Watch: a `pub` signature or
  trait change breaks every caller and every `impl`; …
- Axle: look at calls passing `own`, `mut` or `copy`, `class Foo : Base, Trait`, `impl Type : Trait`,
  test functions, …, error sets `! E` in signatures, … Watch: an error added to a signature's `! E`
  set must be caught or re-declared by every caller; …
```

A language with nothing to say for that playbook (Markdown in a review) is left
out; a project the daemon cannot describe gets the playbook without the
checklist and a line saying how to get one.

An unknown prompt, a missing required `target` or an argument other than
`target`/`scope` is a `-32602`.

## Server instructions

`initialize` carries a short **decision table** (`protocol::INSTRUCTIONS`,
~1.2k chars): which call for which need — `search` vs an `intent` vs a
workflow vs `mode: "keyword"` vs grep, `read` for the rest of a hit or a glob,
your own Read before an edit, `sync {paths}` after one — and that `project` is
detected.

## Token budget

| Lever | Effect |
|--------|--------|
| `intent` | the retrieval task — picking the right one measurably improves ranking; a workflow answers one section per leg |
| `instruction` | free-form task sentence, replacing the intent's |
| `raw_query` | the query verbatim, no instruction, no template |
| `like` | a chunk to find look-alikes of, by its stored vector — nothing embedded |
| `k` | hits per search (default **8**; a workflow keeps each leg's own unless set) |
| `max_snippet_chars` | cap **per excerpt** — default **1200** in compact mode, the daemon's `search.max_snippet_chars` (2400) in full mode |
| `detail` | `compact` shows 3 excerpts per workflow leg, `full` all of them |
| `paths`, `langs`, `kinds`, `symbol`, `scope` | narrow before scoring, not after |
| `mode: "keyword"` | for an exact identifier, no semantics to pay for |

Measured (`crates/mcp/tests/bridge.rs`,
`a_default_search_of_eight_long_hits_stays_inside_its_token_budget`, through
the real daemon): a default `search` over chunks far longer than the cap
answers **8 hits in 10,520 characters ≈ 2.6k tokens** (4 characters a token).

The render-only bound (`render/search_tests.rs`) holds 8 × 1200-character
excerpts within 14,000 characters (≈ 3.5k tokens). The same question resolved
through glob + grep + three `Read`s commonly costs 25,000 to 60,000.

Fixed cost per session: `tools/list` is ~9.2k characters (≈ 2.3k tokens — the
intent guide is most of it), the instructions ~1.2k, `prompts/list` ~2.9k; a
`review` prompt body ~2.3k for a two-language project.

## Errors

A tool that fails returns a **normal result** with `isError: true` and the text
`<tool> failed: <error>`, not a transport error: the model needs to see what
broke and adapt, not receive a JSON-RPC code it can't reason about. A daemon
that is down is such a result too.

A malformed frame returns `-32700` and **does not kill the session**. An
unknown method (`resources/list` included — resources are not served) is
`-32601`; a bad `prompts/get` is `-32602`.

## Protocol

JSON-RPC 2.0, newline-delimited. `initialize` (capabilities `tools` and
`prompts`), `notifications/initialized`, `ping`, `tools/list`, `tools/call`,
`prompts/list`, `prompts/get`.

The version the client announces is echoed back as-is when provided
(`2025-06-18` by default), which keeps older clients working.

Hand-written, ~100 lines: the surface actually needed is small, and writing
it ourselves pins the wire format to something this repo controls rather
than to a client library that revises its API faster than the spec.

## Verifying

`crates/mcp/tests/bridge.rs` drives the bridge with JSON-RPC lines against a
real daemon — router, engine, chunkers, SQLite — served on loopback inside the
test, only the model mocked:

- detection from a manifest and from a registered root
- the no-match error
- `read` by locator and by glob
- a note round-trip
- the re-registration that keeps the manifest's settings
- a sync that answers within its wait or names its job
- a workflow's sections
- the token budget
