# The pipeline — from file to cited hit

## Overview

```
                   ┌──────────────── INDEXING ────────────────┐

   disk           walker           chunker          embedder           store
 ┌────────┐  ┌──────────────┐  ┌────────────┐  ┌───────────────┐  ┌─────────────┐
 │ *.rs   │─▶│ ignore +     │─▶│ RawChunk[] │─▶│ /v1/embeddings│─▶│ vectors     │
 │ *.md   │  │ globs + ext  │  │ (pure)     │  │ (Qwen3 local  │  │ + bucket   │
 └────────┘  └──────────────┘  └────────────┘  │  or remote)   │  └─────────────┘
                    │                 │        └───────────────┘         │
                    │                 │                ▲                 │
             (size,mtime)      ContentHash             │                 │
                    │                 │         cache (recipe,hash) ─────┘
                    ▼                 ▼                │
             gate 1: nothing   gate 2: nothing   gate 3: nothing
             is read           is chunked        is embedded

                   └──────────────── SEARCH ──────────────────┘

   query          embedder          vectors       FTS5/BM25        RRF fusion
 ┌────────┐  ┌──────────────┐  ┌────────────┐  ┌───────────────┐  ┌─────────────┐
 │ "where │─▶│ Instruct: …  │─▶│ cosine     │  │ tokens        │─▶│ 1/(60+rank) │
 │  is    │  │ Query: …     │  │ top-4k     │─▶│ top-4k        │  │ + tie-break │
 │  retry"│  └──────────────┘  └────────────┘  └───────────────┘  └──────┬──────┘
 └────────┘                                                              │
                                             ┌───────────────────────────▼─────┐
                                             │ SearchHit: chunk + locator      │
                                             │ path:12-48 · host_path · score  │
                                             └─────────────────────────────────┘
```

## Stage by stage

### 1. Walk — `findery-index::walker`

`ignore::WalkBuilder` with `.gitignore` honored **even outside a git repo**
(`require_git(false)`): a project root is often a subdirectory, and
`target/` is just as unwanted there. Then an extension filter (`rs`, `md` by
default), then `include`, then `exclude`. A file that's too big is
**reported** in `warnings`, never silently skipped (CONV-022).

Dot-directories are walked **by default** (`include_hidden`): `.github/workflows`
and `.cargo/config.toml` are exactly what people ask about, and skipping them
silently is the opposite of control. `.git` is pruned structurally rather than by
glob — an exclude pattern still walks the loose objects before rejecting them.
Keeping credentials out is then the job of the default `exclude` list, not of the
walker's blindness.

Output sorted by path: a sync is reproducible.

### 2. Chunk — `findery-chunk`

One chunker per language, picked by `Registry`. Pure, no I/O (CONV-025).
Detail: [`../chunk/README.md`](../chunk/README.md).

### 3. Identity — `findery-index::sync::materialize`

L2 is the **single writer** of identity:

- `ChunkId = blake3(project | rel_path | ordinal)` — stable between two
  indexing runs as long as the chunk holds the same position, so an upsert
  cleanly overwrites instead of leaving an orphan.
- `content_hash = blake3(embed_input)` — the hash covers **the text that's
  actually embedded**, `path/lang/symbol` header included. Moving a file
  therefore changes its vector, which is intended: the path is signal.

### 4. Embed — `findery-embed`

A single HTTP client covers TEI, llama.cpp, vLLM, Ollama, LM Studio, OpenAI,
Voyage… Detail: [`../embed/README.md`](../embed/README.md).

### 5. Store — `findery-store`

SQLite bucket (durable, full-text, vector cache) + pluggable vector
backend. Detail: [`../store/README.md`](../store/README.md).

### 6. Search — `findery-index::search`

Two legs, fused by **rank** and not by score: cosine lives in `[-1,1]`,
BM25 is unbounded, calibrating one against the other would be a permanent
tuning job. RRF fuses them with no calibration.

## The three gates — what makes re-sync free

| Gate | Test | What it saves |
|-------|------|--------------------|
| 1 | `(size, mtime)` unchanged | reading the file |
| 2 | `content_hash` unchanged | chunking |
| 3 | `(recipe, content_hash)` cached | **embedding** |

Gate 3 is the one that matters — for whatever the text of the chunk is
identical. Re-run a sync, run a `--full`, re-clone the repository to the same
relative path, or reorder a file: the chunks are the same text, so the vectors
are reused and the GPU stays idle.

**What it does not cover**, because `content_hash` covers the text actually
embedded and that text opens on `path: …`: renaming a file, moving a function
to another file, or renaming a module *file* changes the path, and the chunks
concerned are embedded again. That is deliberate — the path is signal, and
`"where is the rate limit handled"` is answered by
`middleware/rate_limit.rs` as much as by its body. The project's own
`findery-chunk` changes invalidate it the same way: any change to how a file
is split re-embeds what it split.

Executable proof: `crates/server/tests/api.rs`,
`a_second_sync_costs_no_embedding_at_all` and
`a_full_sync_still_reuses_the_content_addressed_cache`.

## What changes a vector

The cache namespace is `recipe = "<model>:<dim>"`. Changing model or
dimension opens a **new** space, and a bucket refuses to mix two recipes —
at the operation, never at boot. This is deliberate: a corpus half embedded
at 1024 dims and half at 2560 is silently unsearchable, so the mix has to be
refused; but refusing it at boot would also take down the keyword search and
the `bucket rm` that repairs it. Detail:
[`../embed/README.md`](../embed/README.md).
