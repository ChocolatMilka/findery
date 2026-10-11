# `findery-embed` — the model, and nothing hardcoded

## The principle

No model is compiled into the binary. The backend, the URL, the route, the
auth header, the model name, and the vector width are **all** data. The
defaults describe the bundled Qwen3 container — pointing `base_url` at
`api.openai.com`, at a remote TEI behind TLS, or at the Ollama on the
machine next door is a config change, not code.

A single HTTP client (`OpenAiCompatEmbedder`) covers everything that serves
`/v1/embeddings`: TEI, llama.cpp, vLLM, Ollama, LM Studio, Infinity, OpenAI,
Voyage, Jina, Azure. It accepts **two** response shapes — the OpenAI
envelope and TEI's bare `/embed` matrix — because the same code has to talk
to both.

## Typical configurations

| Target | Settings |
|-------|----------|
| DashScope (default) | `FINDERY_EMBED_BASE_URL=https://{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`, `FINDERY_EMBED_MODEL=qwen3.7-text-embedding`, `FINDERY_EMBED_DIM=1024`, `FINDERY_EMBED_API_KEY=$DASHSCOPE_API_KEY` |
| A local runtime instead | `FINDERY_EMBED_BASE_URL=http://127.0.0.1:1234`, `FINDERY_EMBED_MODEL=<whatever it serves>` — `qwen3.7-text-embedding` is API-only, so a local box runs a different model and therefore a different recipe |
| llama.cpp (GGUF) | `FINDERY_EMBED_BASE_URL=http://llama:8080` |
| OpenAI | `FINDERY_EMBED_BASE_URL=https://api.openai.com`, `FINDERY_EMBED_MODEL=text-embedding-3-large`, `FINDERY_EMBED_DIM=3072`, `FINDERY_EMBED_API_KEY=sk-…`, `FINDERY_EMBED_SEND_DIMENSIONS=true` |
| Azure OpenAI | + `FINDERY_EMBED_AUTH_HEADER=api-key`, `FINDERY_EMBED_AUTH_PREFIX=` (empty) |
| Remote GPU | `FINDERY_EMBED_BASE_URL=https://embed.interne`, `FINDERY_EMBED_API_KEY=…` |

`FINDERY_EMBED_HEADERS="Name: value; Other: value"` adds static headers
(gateways, tenancy, API versioning).

## The Qwen3 asymmetry — don't break this

qwen3.7-text-embedding is **instruction-aware and asymmetric**:

```text
Instruct: {task}
Query: {question}
```

on the **query** side, and raw text on the **document** side. Prefixing the
documents too costs a few MTEB points and, worse, doubles the blast radius
of a prompt change: every stored vector would need recomputing.

The formatting lives in `findery_core::prompt` (CONV-012). Nobody concatenates
`"Instruct: "` anywhere else.

### The presets — what an agent can actually choose

Asking a model to invent a good instruction on every call produces a
paraphrase of the query, which is **worse** than the default: it duplicates
the signal the query already carries instead of describing the TASK. Hence
named intents, defined in `QueryTask` (L0, CONV-012):

| `task` | For | The instruction describes |
|--------|------|----------------------|
| `code` *(default)* | "where is X handled" | code or docs that answer it |
| `symbol` | a name in hand | the declaration, the type, the docs |
| `usage` | an API | the call sites |
| `docs` | an intent | the rationale, not the implementation |
| `test` | a behavior | the tests that assert it |
| `config` | a setting | declaration, default, where it's read |
| `error` | a message, a panic | what produces it and what handles it |

Precedence, resolved in `SearchRequest::resolved_instruction()` in L0 so the
three transports can't diverge:

```
instruction (free text)  >  task (preset)  >  query_instruction (config)
```

Exposed everywhere: HTTP body (`task`, `instruction`), `findery-client`, the MCP
tool `search_code`, and `findery search --task symbol` / `--instruct "…"`.

## The dimension probe

At startup, the client sends **one** throwaway entry and compares the
width it gets back to `dim`. If they diverge, the daemon refuses to start.

This isn't ceremony: a corpus half embedded at 1024 dims and half at 2560
is silently unsearchable, and the only cheap moment to catch that is before
the first byte is written. `dim = 0` means "ask the runtime". `verify_dim =
false` disables the probe if startup needs to be offline.

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
| the bucket's recorded recipe, when the model name still matches | starts degraded |
| nowhere | refuses, naming `FINDERY_EMBED_DIM` |

Degraded means: the daemon is up, `/healthz` answers, `/v1/health` reports
`embedder_ok: false` with the transport error, searches and syncs fail with that
same error, and everything else works. `findery doctor` reads exactly this and
tells the user to start their model.

INTENT: the daemon starts itself now, on the user's first command. A daemon that
refused to boot because the model was down would take the diagnostic down with it
at precisely the moment it is needed, and the user would see "did not answer
within 30s" instead of "your model is not running".

## The cache recipe

```
recipe = "<model>:<dim>"
```

This is the vector cache's namespace. Changing model or dimension opens a
**new** space, and a bucket refuses to mix two recipes:

```
bucket `default` holds vectors built with `qwen3.7-text-embedding:1024`, but
this daemon serves `text-embedding-3-large:3072`; re-index it — `findery bucket
rm default` then `findery init --bucket default` — or start the daemon with the
embedder settings it was built with
```

**The refusal lands on the operation, never on the boot.** A bucket whose
recipe disagrees is opened anyway; `BucketHandle::conflicted` records the
disagreement and `ensure_vectors_usable` raises it at every operation that would
mix two vector spaces — the vector leg of a search, and each of the three write
entry points: `sync::run`, `index_inline` and `push_documents`. Everything that
touches no vector keeps working: keyword search, project listing, `findery
bucket ls` (which prints the conflict), and `findery bucket rm`.

INTENT: refusing at boot removed the very door the message recommends. The
daemon never came up, so the `rm` the user was told to run could not run, and
one stale bucket took the whole index — including the BM25 leg that was still
perfectly good — down with it.

The write half matters as much as the read half, and the store's own width check
cannot cover it: `SqliteVectorStore::warm` compares dimensions, so it sees a
`1024`-dim corpus meeting a `2560`-dim embedder, but it is blind to two models
that happen to share a width. Only the recipe comparison catches that one, and
it has to be asked before the first write — a sync that reaches `flush` has
already put the new space's vectors beside the old ones.

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
and an offline diagnostic. It's **never** a silent fallback: it's only
reached with an explicit `backend = "mock"` (CONV-036).

## Choosing a size

| Model | Dim | Weight | Where |
|--------|-----|-------|-----|
| `qwen3.7-text-embedding` | 256 → 2560, default 1024 | none, it is an API | **the default.** 131K context, $0.07/1M tokens |
| Qwen3-Embedding-0.6B | 1024 | ~1.2 GB | a local fallback when an API is not an option |
| Qwen3-Embedding-8B | 4096 | ~16 GB | a local fallback, GPU |

The dimension multiplies everything downstream: matrix RAM, cache size,
scan duration. 1024 is the right starting point; go up when a measurement
demands it, not before.
