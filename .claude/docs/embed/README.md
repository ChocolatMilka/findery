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
| Local TEI (default) | `FINDERY_EMBED_BASE_URL=http://tei:80`, `FINDERY_EMBED_MODEL=Qwen/Qwen3-Embedding-0.6B`, `FINDERY_EMBED_DIM=1024` |
| Ollama on the host | `FINDERY_EMBED_BASE_URL=http://host.docker.internal:11434`, `FINDERY_EMBED_MODEL=qwen3-embedding:0.6b` |
| llama.cpp (GGUF) | `FINDERY_EMBED_BASE_URL=http://llama:8080` |
| OpenAI | `FINDERY_EMBED_BASE_URL=https://api.openai.com`, `FINDERY_EMBED_MODEL=text-embedding-3-large`, `FINDERY_EMBED_DIM=3072`, `FINDERY_EMBED_API_KEY=sk-…`, `FINDERY_EMBED_SEND_DIMENSIONS=true` |
| Azure OpenAI | + `FINDERY_EMBED_AUTH_HEADER=api-key`, `FINDERY_EMBED_AUTH_PREFIX=` (empty) |
| Remote GPU | `FINDERY_EMBED_BASE_URL=https://embed.interne`, `FINDERY_EMBED_API_KEY=…` |

`FINDERY_EMBED_HEADERS="Name: value; Other: value"` adds static headers
(gateways, tenancy, API versioning).

## The Qwen3 asymmetry — don't break this

Qwen3-Embedding is **instruction-aware and asymmetric**:

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

## The cache recipe

```
recipe = "<model>:<dim>"
```

This is the vector cache's namespace. Changing model or dimension opens a
**new** space, and the daemon refuses to mix two recipes in the same
catalog:

```
this catalog was built with embedding recipe `Qwen/Qwen3-Embedding-0.6B:1024`
but the daemon now offers `text-embedding-3-large:3072`; keep the old settings,
point FINDERY_DATA_DIR at a fresh directory, or delete the catalog to re-index
```

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
| Qwen3-Embedding-0.6B | 1024 | ~1.2 GB | comfortable on CPU — the default |
| Qwen3-Embedding-4B | 2560 | ~8 GB | GPU |
| Qwen3-Embedding-8B | 4096 | ~16 GB | GPU, max quality |

The dimension multiplies everything downstream: matrix RAM, cache size,
scan duration. 1024 is the right starting point; go up when a measurement
demands it, not before.
