# `findery-embed` — the model, and nothing hardcoded

## The principle

No model is compiled into the binary. The backend, the URL, the route, the
auth header, the model name, and the vector width are **all** data.

The built-in defaults point at DashScope's `qwen3.7-text-embedding`, and the
bundled TEI container serves Harrier. Pointing `base_url` at `api.openai.com`,
at a remote TEI behind TLS, or at the Ollama on the machine next door is a
config change, not code.

A single HTTP client (`OpenAiCompatEmbedder`) covers everything that serves
`/v1/embeddings`: TEI, llama.cpp, vLLM, Ollama, LM Studio, Infinity, OpenAI,
Voyage, Jina, Azure.

It accepts **two** response shapes — the OpenAI envelope and TEI's bare
`/embed` matrix — because the same code has to talk to both.

## Typical configurations

| Target | Settings |
|---|---|
| DashScope (default) | `FINDERY_EMBED_BASE_URL=https://{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`, `FINDERY_EMBED_MODEL=qwen3.7-text-embedding`, `FINDERY_EMBED_DIM=1024`, `FINDERY_EMBED_API_KEY=$DASHSCOPE_API_KEY` |
| A local runtime instead | `FINDERY_EMBED_BASE_URL=http://127.0.0.1:1234`, `FINDERY_EMBED_MODEL=<whatever it serves>` — `qwen3.7-text-embedding` is API-only, so a local box runs a different model and therefore a different recipe |
| The native runtime (`runtime/`) | `FINDERY_EMBED_BASE_URL=http://127.0.0.1:8090`, `FINDERY_EMBED_MODEL=harrier-oss-v1-0.6b` — see below |
| llama.cpp (GGUF) | `FINDERY_EMBED_BASE_URL=http://llama:8080` |
| OpenAI | `FINDERY_EMBED_BASE_URL=https://api.openai.com`, `FINDERY_EMBED_MODEL=text-embedding-3-large`, `FINDERY_EMBED_DIM=3072`, `FINDERY_EMBED_API_KEY=sk-…`, `FINDERY_EMBED_SEND_DIMENSIONS=true` |
| Azure OpenAI | + `FINDERY_EMBED_AUTH_HEADER=api-key`, `FINDERY_EMBED_AUTH_PREFIX=` (empty) |
| Remote GPU | `FINDERY_EMBED_BASE_URL=https://embed.interne`, `FINDERY_EMBED_API_KEY=…` |

`FINDERY_EMBED_HEADERS="Name: value; Other: value"` adds static headers
(gateways, tenancy, API versioning).

## The native runtime — `runtime/`

findery never runs a model itself; [`runtime/`](../../../runtime/) is the
shortest path to one that is local, on the GPU, without Docker:
llama.cpp's `llama-server` in embedding mode, behind llama-swap, which serves
every model on one port (`127.0.0.1:8090`) and routes by the request's `model`.
An unknown name is an error, never another model's vectors.

| File | What it is |
|---|---|
| `models.ps1` / `models.sh` | the same verbs on Windows and Linux: `install`, `pull`, `start`, `stop`, `restart`, `status`, `ui`, `logs`, `devices`, `autostart on\|off`, `uninstall` |
| `pins.txt` | the llama.cpp build, the llama-swap version, and the sha256 of every asset and model file — the only place a version is decided |
| `llama-swap.yaml` | the template `install` fills in once; the copy in the runtime directory is the user's to edit and is never overwritten |
| `findery-models.service` | the systemd user unit `autostart on` writes on Linux; Windows uses a `Run` key that starts the runtime hidden at logon |

Why it is shaped this way:

- **Pinned, then checked.** A runtime that installs "latest" changes under a
  working setup. Every download is verified against `pins.txt` before it is
  unpacked, and a step already done is skipped, so `install` is safe to run
  again.
- **The served names are LM Studio's** (`harrier-oss-v1-0.6b`,
  `text-embedding-qwen3-embedding-0.6b`), and the GGUF files sit in LM Studio's
  layout (`owner/repo/file`). The recipe is `<model>:<dim>`, so a project
  indexed through LM Studio keeps every vector when its URL moves to `:8090` —
  and an existing LM Studio library is reused instead of downloaded again.
- **Last-token pooling, 4 slots of 8k tokens**: what Qwen3-Embedding and Harrier
  expect, and room for a sync's concurrent batches. Both models stay loaded
  together (`swap: false`), so two projects on two models never evict each
  other; a model idle for 30 minutes unloads and reloads on the next request.
- **A copy of the tooling lives beside the binaries**
  (`<runtime>/findery-models[.ps1]`, `pins.txt`, `templates/`), so the service
  and autostart keep working whatever happens to the repository checkout.

The runtime directory is `<data_dir>/runtime` (`FINDERY_RUNTIME_DIR` moves it,
`FINDERY_MODELS_LISTEN` changes the port). `tools/` is the repository's own
development tooling; `runtime/` is for users, which is why it sits apart.

## Model profiles

`[embedder]` is a profile — the implicit one, named `embedder`. Every
`[models.<alias>]` is another, written as **the difference from `[embedder]`**
and merged over it by `Config::profiles` (`config/models.rs`):

```toml
default_model = "harrier"            # or FINDERY_MODEL; absent = `embedder`

[embedder]                           # unchanged: still the implicit profile
base_url = "http://localhost:1234"
model = "text-embedding-qwen3-embedding-0.6b"

[models.harrier]                     # only what differs
model = "harrier-oss-v1-0.6b"
```

| Rule | Why |
|---|---|
| a profile inherits every key it does not set | most profiles of one machine share a runtime, a batch size, a timeout |
| `api_key`, `extra_headers`, `auth_header`, `auth_prefix` are **not** inherited once the profile names another `base_url` | a DashScope key must never be sent to LM Studio |
| `weights`, `query_template`, `document_template` are **not** inherited once the profile names another `model` | another model's `weights` would put two models in one vector space |
| a profile is re-read through `EmbedderConfig`'s `deny_unknown_fields` | a typo under `[models.x]` is refused like one under `[embedder]` |
| the alias is letters, digits, `-`, `_`, starting with a letter or digit, ≤ 64 (`ModelAlias`) — the bucket-name charset **without `.`**; `embedder` is reserved | an alias is a TOML key and one segment of `findery config set models.<alias>.<key>`, where a `.` would split it: `qwen3` is valid, `qwen3-0.6b` and `../x` are not |
| an unknown `default_model` is `Error::ModelUnknown` (400), naming the known aliases | at boot, and `Config::validate` checks every profile |

### Several embedders at once — the pool

The daemon serves **every** profile, through `findery-index`'s `EmbedderPool`
(`models.rs`):

| Profile | Built |
|---|---|
| the default one — `default_model`, else `[embedder]` | at boot, with the degraded-boot rule below; a failure refuses the boot, as it always did |
| every other | on its first use, through a `OnceCell` that does **not** cache a failure: a runtime started after the first attempt is picked up by the next one, with no restart |

An operation on a project uses the project's **pin** (`model = "…"` in its
manifest, `findery init --model`, `RegisterProject.model`, `ProjectSpec.model`)
or, without one, the default profile.

`Indexer::set_default_model(alias)` (`POST /v1/models/default`,
`findery model use`, which also saves `default_model`) switches the default
**from the next operation on**: nothing is rebuilt or moved, the other model's
bucket files stay where they are, and switching back costs nothing — its
vectors never left.

An alias no profile defines is `ModelUnknown` (400), naming the known ones, and
changes nothing. A pin is validated the same way at registration, before
anything is written.

A profile has a vector space **before** it has an embedder: its recipe is
computable from its configured `dim`, or from the width a bucket on disk
remembers for its identity (the boot scan reads every member's recipe).

That is what lets a read find the right bucket file without dialing a runtime
nobody started; only a profile whose width is known from nowhere is built to
learn it.

`Indexer::models()` (`GET /v1/models`) lists every profile without probing
anything; `/v1/health` adds a probe of the ones already built.

`FINDERY_MODEL=harrier` sets the default at boot. A configuration without
`default_model` behaves byte for byte as before profiles existed.

`config/findery.toml` ships two profiles for LM Studio, inert until selected:
`harrier` (`harrier-oss-v1-0.6b`) and `qwen3`
(`text-embedding-qwen3-embedding-0.6b`).

### Walkthrough — LM Studio, qwen3 → harrier and back

LM Studio on `http://localhost:1234` serves both models, both 1024 wide, so
the width cannot tell them apart; only the recipe does.

The setup: `[embedder]` names `text-embedding-qwen3-embedding-0.6b` (the index
every project already has), and the two shipped profiles sit beside it.

```bash
findery model ls                 # embedder and qwen3 share tag 47m32eui: one vector space
findery model use harrier        # now, no restart; saved as default_model
cd D:/work/axle && findery sync  # creates .findery/bucket.harrier-oss-v1-0-6b-zvpmtsdf.db
                                 # re-chunks (pure, cheap) and embeds under harrier
findery search "where is retry handled"   # harrier's vectors
findery model use qwen3          # back: the next search reads bucket.…-47m32eui.db
findery sync                     # embeddings_computed = 0 — nothing is re-embedded
```

Between the switch and the first sync under harrier, a search in a project
answers keyword hits with one `keyword results only` line naming the model and
`findery sync -p <id>`; `findery doctor` lists every project in that state.

`embedder` and `qwen3` name the same model with no `weights`, so they resolve
to the same recipe and the same file: switching between those two is free in
both directions from the start.

`findery bucket ls` shows both files of each bucket, the `serving` one marked;
`findery bucket rm axle --model harrier` drops the harrier file alone if the
experiment is over.

### `weights` — the identity of what is served

`weights` names the weights behind `model` when the runtime's own name is not a
canonical one. It replaces `model` in the recipe, so LM Studio's
`harrier-oss-v1-0.6b` and TEI's `microsoft/harrier-oss-v1-0.6b` — one set of
weights under two names — share their vectors.

A GGUF quantization (`Q8_0`, `Q4_K_M`) is **different** weights: give it its
own name, or leave `weights` unset. `FINDERY_EMBED_WEIGHTS` sets it on
`[embedder]`.

## The prompt format — a fact about the weights

Instruction-aware models (Qwen3-Embedding, Harrier, qwen3.7-text-embedding) are
**asymmetric**: an instruction on the **query** side, raw text on the
**document** side. Prefixing the documents too costs a few MTEB points and,
worse, doubles the blast radius of a prompt change: every stored vector would
need recomputing.

Both sides are templates on the profile, rendered only in
`findery_core::embedding::prompt` (CONV-012) — nobody concatenates `"Instruct: "` anywhere
else:

| Setting | Default | Must contain | nomic-style |
|---|---|---|---|
| `query_template` | `Instruct: {instruction}\nQuery: {query}` | `{query}`; `{instruction}` (or the model cards' `{task}`) optional | `search_query: {query}` |
| `document_template` | `{text}` (the identity) | `{text}` | `search_document: {text}` |

- An unknown placeholder (`{qurey}`) is refused, not embedded verbatim; a
  brace that does not enclose a bare lowercase word stays literal.
- An empty instruction degrades to the raw query when the template has an
  instruction slot; a template without one is always rendered.
- The default template renders exactly the bytes `format_query` rendered
  before templates existed (a test holds the old implementation verbatim).
- `FINDERY_EMBED_QUERY_TEMPLATE` / `FINDERY_EMBED_DOCUMENT_TEMPLATE` override
  `[embedder]`'s; a literal `\n` in them stands for a newline.

The document template is applied **in the embedder**, to
`Chunk::embed_input()` — after the chunk's `content_hash` was computed. The
hash therefore does not see it, which is why a non-identity
`document_template` enters the **recipe** instead (`|doc=…`, below): the cache
key `(recipe, content_hash)` still names exactly the text that was sent.

The default instruction is language-neutral ("Given a developer question about
a codebase, …"): one daemon indexes Rust, Axle, PHP and prose side by side.
Changing it moves query vectors only, never a stored one.

### The intent library — what an agent can actually choose

Asking a model to invent a good instruction on every call produces a
paraphrase of the query, which is **worse** than the default: it duplicates
the signal the query already carries instead of describing the TASK.

Hence named **intents** (`findery_core::intent`, L0). They are **data**:
`crates/core/intents.toml`, embedded with `include_str!` and parsed once — a
unit test parses the same bytes, and `Config::validate` refuses to boot on a
library that does not load.

Each intent carries:

- a `name`, `aliases`
- a one-line `summary` (for an agent choosing)
- a `kind`
- ONE instruction sentence ("Given …, retrieve …", language-neutral)
- per-language `hints`
- `pairing` / `duplicates` flags for a workflow
- one or more **legs** —
  `{ label, intent?, instruction?, kinds?, langs?, exclude_langs?, mode?, k? }`

A leg that names `intent = "usage"` borrows that locate intent's sentence,
hints and restrictions.

**Locate intents** — one leg:

| Intent | Aliases | For |
|---|---|---|
| `code` *(default)* | | where and how something is done |
| `symbol` | `definition`, `declaration` | a name in hand: declaration, signature, doc comment |
| `usage` | `callers`, `uses`, `references` | call sites, instantiations, imports, wiring |
| `implementations` | `impls`, `implementors`, `subclasses`, `overrides` | what implements, extends or overrides a trait, interface, base |
| `api` | `entrypoints`, `endpoints`, `surface` | routes, handlers, CLI commands, exported items |
| `flow` | `trace`, `dataflow` | entry point → each hop → sink |
| `docs` | `doc`, `documentation`, `rationale` | the why: READMEs, design notes, doc comments |
| `test` | `tests` | the tests that assert a behaviour |
| `config` | `configuration`, `settings`, `env` | declared, defaulted, validated, read |
| `error` | `diagnostic`, `errors`, `exception` | what raises it, handles it, defines it |
| `similar` | `duplicates`, `clones` | the same logic elsewhere, diverged copies included |

**Workflow intents** — several legs:

| Intent | Legs |
|---|---|
| `review` (`impact`) | `callers` (usage) · `tests` (test) · `docs` (docs) · `similar` (similar) |
| `refactor` (`rename`) | `definition` (symbol) · `usages` (usage) · `implementations` · `tests` · `duplicates` (similar) |
| `doc-sync` (`drift`) | `docs` (markdown, `doc_section`) ↔ `code` (everything but markdown); `pairing` |
| `divergence` (`diverged`, `inconsistencies`) | `docs` ↔ `code`, then `duplicates`; `pairing` |
| `onboard` (`overview`, `tour`) | `overview` (docs) · `entry-points` (api) · `types` |
| `security` (`audit`) | `input` · `auth` · `unsafe` · `injection` · `secrets` |

`pairing` matches each hit of the first leg with its nearest hits of the
second, by stored vector; `duplicates` reports near-identical code across files
among the hits. Both run in `Indexer::search` — see `index/README.md`.

#### Languages in the instruction

Two placeholders name the languages a search touches, and render a plain noun
when none is known:

```text
"Given an API, function or type of {codebase}, retrieve …"
  [rust, php]       → "… of a Rust and PHP codebase, retrieve …"
  [axle]            → "… of an Axle codebase, retrieve …"
  []                → "… of a codebase, retrieve …"
"Given a question about {project}, retrieve …"   → "an Axle project", "a project"
```

Any other `{word}` and a line break are refused when the library is built;
other braces (`{ }`, `#[cfg(test)]`) stay literal. Markdown, TOML and JSON never
name a codebase.

`hints` are short lists of constructs per language, joined at render time into
one clause per language, in the caller's priority order, inside the same
sentence:

```toml
[intent.hints]
rust = ["call sites", "`impl Trait for` blocks", "generic bounds", "macro invocations", "`use` paths"]
```

```text
…; Rust: call sites, `impl Trait for` blocks, generic bounds, macro invocations and `use` paths; PHP: ….
```

They name each language's real constructs for the task — `impl Trait for` for
Rust usage, `! E` error sets for Axle errors, `if err != nil` for Go, `@Test`
for Java and Kotlin, pytest fixtures for Python.

`search.max_instruction_chars` (default 640) caps the whole: the sentence is
never cut, and the hints kept are always a prefix of the priority order (`0`
keeps none).

The neutral rendering of `code` is exactly `DEFAULT_QUERY_INSTRUCTION`, so
naming nothing and naming `code` with no known language send the same bytes.

#### Layers and precedence

Three layers, each one overriding the one before:

1. `IntentLibrary::builtin()`
2. the daemon's `[intents.<name>]` (`Config.intents`, `Config::intent_library`)
3. the manifest's `[intents.<name>]` (`ProjectConfig.intents`, carried on
   `ProjectSpec.intents` at registration, where the daemon validates it before
   writing anything)

A key names an intent or an alias. Within a layer:

- every field a key sets replaces that field
- `hints` merge language by language (an empty list removes one)
- `legs` replace the whole plan
- a key that names nothing defines a new intent and must set `summary` and
  `instruction`

Each layer is re-validated as a whole, so an override of `usage` reaches the
`review` leg that borrows it. Unknown keys are refused (`deny_unknown_fields`),
at the moment the file is read.

The engine keeps each project's layered library with the overrides it was built
from, and rebuilds it only when the spec carries different ones.

The ladder, resolved in ONE place — `SearchRequest::resolve_plan` — so three
transports cannot diverge:

```text
raw_query  >  instruction  >  intent / task  >  like (runs `similar`)
           >  project instruction  >  project task  >  query_instruction
```

- `raw_query: true` sends the query verbatim: `QueryPrompt::Raw`, which every
  embedder renders through `prompt::render_query` — no instruction, no
  template, whatever the profile's `query_template`.
- A free `instruction` replaces every leg's sentence; the intent still decides
  the legs.
- `intent` and `task` are the same namespace (`task` keeps its closed set —
  `code`, `symbol`, `usage`, `docs`, `test`, `config`, `error` and the aliases
  `definition`, `callers`, `doc`, `tests`, `configuration`, `diagnostic`, a 422
  for anything else). Naming two different intents through them is refused.
- An unknown `intent` is `Error::Invalid` (400) listing the known names —
  never a silent fallback.
- `like` with no intent runs `similar`; with an intent, it feeds that intent's
  `similar` legs, and an intent without one is refused. A `like` leg embeds
  nothing (`by_example`).

Filters narrow, never widen: the request's filter is laid over the scope
(`SearchFilter::layer`), then each leg intersects its `langs` / `kinds` with it
and subtracts its `exclude_langs`.

A leg left with nothing (`doc-sync` with `langs = ["rust"]` loses its markdown
leg) is dropped with a warning, and `pairing` is lifted; a filter that empties
every leg is `Error::Invalid`. A leg's `k` and `mode` apply unless the request
names its own.

The instruction names the filter's languages when they were chosen, otherwise
the project's language mix — its files' languages by chunk count
(`Bucket::lang_mix`) — minus what the leg excludes.

The other half of the pair, `query`, is never defaulted and never derived: the
model is handed the two as `Instruct: {…}` and `Query: {…}`, and a client that
can send one and not the other cannot express the format the model documents.

The project's own entry is the manifest's `instruction` / `task`. It exists
because "how should this corpus be asked about" is a property of the corpus,
and a team should be able to commit that opinion rather than pass it on every
call.

`SearchRequest` is `deny_unknown_fields`: `{"instructions": …}` is a 422, not a
search that silently ignored its caller.

Exposed:

- the HTTP body (`task`, `intent`, `instruction`, `raw_query`, `like`)
- `GET /v1/intents`
- `findery-client`
- the MCP tool `search` (`intent` as an enum of the built-in names,
  `instruction`, `raw_query`, `like` — not the legacy `task`, which `intent`
  covers)
- `findery search --intent … --instruct … --raw --like …` / `findery intents`

Every response echoes the resolution (`resolved`).

## The dimension probe

At startup, the client sends **one** throwaway entry and compares the
width it gets back to `dim`. If they diverge, the daemon refuses to start.

This isn't ceremony: a corpus half embedded at 1024 dims and half at 2560
is silently unsearchable, and the only cheap moment to catch that is before
the first byte is written.

- `dim = 0` means "ask the runtime".
- `verify_dim = false` disables the probe if startup needs to be offline.

### When the runtime does not answer

An **unreachable** runtime is a different case from a **disagreeing** one, and
they get different answers.

A width that disagrees is a contradiction the daemon cannot resolve, so it
refuses to start. A runtime that is merely down tells us nothing about the
width — so if the width is known from somewhere else, the daemon starts
*degraded*:

| Width known from | Result |
|---|---|
| `dim` in the configuration | starts degraded |
| the bucket's recorded recipe, when its identity (`weights`, else the model name) still matches | starts degraded |
| nowhere | refuses, naming `FINDERY_EMBED_DIM` |

Degraded means:

- the daemon is up, `/healthz` answers
- `/v1/health` reports `embedder_ok: false` with the transport error — and, per
  profile, `models[]` with `connected` and `ok`
- a **hybrid** search answers keyword hits with a warning naming the model
  (`.claude/docs/index/README.md` §2)
- a vector search and a sync that has something to embed fail with that same
  error
- everything else works (a sync whose every chunk is cached writes with the
  runtime down)

`findery doctor` reads exactly this and tells the user to start their model. A
non-default profile whose runtime is down is reported the same way, without
stopping anything.

INTENT: the daemon starts itself, on the user's first command. A daemon that
refused to boot because the model was down would take the diagnostic down with
it at precisely the moment it is needed, and the user would see "did not answer
within 30s" instead of "your model is not running".

## The cache recipe

```text
recipe = "{identity}[|doc={12 hex}]:{dim}"
```

Built only by `Recipe::of` (`findery_core::embedding::recipe`, CONV-013), once per
profile, by the embedder pool (`EmbedderPool::recipe_of`):

| Part | Value |
|---|---|
| `identity` | the profile's `weights` when set, else the model name the embedder reports (`mock` for the mock backend) |
| `\|doc=` | the first 12 hex of `blake3(document_template)`, present **only** for a non-identity template |
| `dim` | the width actually served — never `0`, which `Recipe::of` refuses |

So a profile with no `weights` and no `document_template` yields exactly
`"<model>:<dim>"` — `text-embedding-qwen3-embedding-0.6b:1024` for the LM
Studio setup — the bytes every existing bucket carries: no cached vector is
lost.

An identity that is empty or contains `|` is refused. `Recipe::parse` splits
the width off the **last** `:`, so an Ollama `nomic-embed-text:latest`
survives.

Two short forms derive from a recipe:

- **`RecipeTag`** — `blake3::derive_key("findery recipe tag v1", recipe)`, first
  5 bytes, RFC 4648 base32 **lowercase** without padding: 8 characters of
  `a–z2–7`, safe on case-folding file systems. The values are frozen by a test
  (`text-embedding-qwen3-embedding-0.6b:1024` → `47m32eui`,
  `harrier-oss-v1-0.6b:1024` → `zvpmtsdf`); changing one orphans every user's
  bucket files.
- **`slug`** — cosmetic only, never parsed: the identity's last `/` segment,
  lowercased, every other character a `-`, at most 40 characters, `model` when
  nothing is left (`Qwen/Qwen3-Embedding-0.6B` → `qwen3-embedding-0-6b`).

`config::bucket_member_path(base, recipe)` spells the file one recipe's member
of a bucket will live in, `{dir}/{stem}.{slug}-{tag}.db`, and
`parse_member_name` reads the tag back (a slug never holds a `.`, so
`bucket.foo.x-abcdefgh.db` is not a member of `bucket`).

The store's `layout` and `adopt` modules build and read them
(`.claude/docs/store/README.md` §0), and the engine routes every operation
through them: one member file per recipe in each bucket
(`.claude/docs/index/README.md` §0).

This is the vector cache's namespace. Changing model or dimension opens a
**new** space — a new member file beside the old one, never a refusal.

What is still refused, with `Error::RecipeMismatch` (HTTP 409, code
`recipe_mismatch`), is a member file whose content contradicts its name: one
renamed by hand, or the one-in-10¹⁰ tag collision. The file's own
`meta.embedding_recipe` is compared with the recipe every operation asks for
(`BucketHandle::ensure_serves`), and `Error::misnamed_member` names both
repairs:

```text
findery: recipe mismatch: bucket `work` holds vectors built with
`another-model:1024` in …/buckets/work/bucket.mock-46p6n2oo.db, a file named for
`mock:1024`. To keep them: rename the file to
…/buckets/work/bucket.another-model-<tag>.db. To re-index it under `mock:1024`:
`findery bucket rm work --force`, then `findery init --bucket work --sync` from
the root of each project it held
```

The rebuild half depends on where the bucket's file is, and both
`Error::misnamed_member` and `Error::recipe_mismatch` pick it from that one
fact:

| Bucket | Repair the message names |
|---|---|
| at the path its name derives (`<data_dir>/buckets/<name>/bucket.db`) | `findery bucket rm <name> --force`, then `findery init --bucket <name> --sync` from each project root |
| anywhere else — inside a project (the default), or at an explicit path | `findery bucket rm <name> --force`, then `findery init --sync` from each project root; the manifest puts the bucket back where it was |
| `default` | move each project out with `findery init --bucket <new-name> --sync` — `default` cannot be removed |

INTENT: `findery init --bucket <name>` re-creates a bucket **at the path the
name derives**. Recommended for a bucket inside a project, it would silently
move the project out of its own tree into the data directory.

**The refusal lands on the operation, never on the boot.** A member whose
recipe disagrees is opened anyway; `BucketHandle::conflicted` records the
disagreement and `ensure_serves` raises it at every operation that would mix
two vector spaces:

- the vector leg of a search (which, in hybrid mode, degrades to keyword hits
  and a warning)
- registration into the bucket (`Indexer::register` asks before the project
  moves and before the store warms)
- each of the three write entry points: `sync::run`, `index_inline` and
  `push_documents`

Everything that touches no vector keeps working: keyword search, project
listing, `findery bucket ls` (which prints the conflict), and
`findery bucket rm`.

INTENT: refusing at boot removed the very door the message recommends. The
daemon never came up, so the `rm` the user was told to run could not run, and
one stale bucket took the whole index — including the BM25 leg that was still
perfectly good — down with it.

The write half matters as much as the read half, and the store's own width check
cannot cover it: `SqliteVectorStore::warm` compares dimensions, so it sees a
`1024`-dim corpus meeting a `2560`-dim embedder, but it is blind to two models
that happen to share a width.

Only the recipe comparison catches that one, and it has to be asked before the
first write — a sync that reaches `flush` has already put the new space's
vectors beside the old ones.

The width check is the net for the conflict the recipe failed to record (a
bucket stamped before recipes were). It raises the same `RecipeMismatch`, built
by `Error::width_mismatch`, with a per-project repair — `findery rm -p <id>`,
then `findery init --sync` from the project root.

Never `findery sync --full`: gate 3 keeps every stored vector, so a full pass
meets the old width again.

The width check holds a warm matrix to the same width as a cold one, so an
off-width upsert is refused rather than written beside rows it cannot be
compared with. Qdrant's collection-width check raises the same variant.

## Throughput, batching, retries

- `batch_size` entries per HTTP request, `concurrency` batches in flight.
- A semaphore bounds concurrent batches: a big sync shouldn't hammer a
  shared or rate-limited endpoint.
- `buffered` preserves input order — that's what lets the caller zip the
  vectors back onto its chunks without carrying an index around.
- Exponential retries (200 ms × 2ⁿ, capped at 5 s) on timeout, connection
  error, 429, and 5xx. A 400 isn't retried: it won't heal.
- Entries truncated at `max_input_chars`, on a character boundary.

## The `mock` backend

Deterministic vectors derived from blake3, normalized. It's used for tests
and an offline diagnostic.

It's **never** a silent fallback: it's only reached with an explicit
`backend = "mock"` (CONV-036).

## Choosing a size

| Model | Dim | Weight | Where |
|---|---|---|---|
| `qwen3.7-text-embedding` | 256 → 2560, default 1024 | none, it is an API | **the default.** 131K context, $0.07/1M tokens |
| Qwen3-Embedding-0.6B | 1024 | ~1.2 GB | a local fallback when an API is not an option |
| Harrier (`microsoft/harrier-oss-v1-0.6b`) | 1024 | ~1.2 GB | local, MIT, Qwen3-based and multilingual; same `Instruct:`/`Query:` query format, documents raw — so the default templates fit. The bundled TEI's default |
| Qwen3-Embedding-8B | 4096 | ~16 GB | a local fallback, GPU |

The dimension multiplies everything downstream: matrix RAM, cache size,
scan duration.

1024 is the right starting point; go up when a measurement demands it, not
before.
