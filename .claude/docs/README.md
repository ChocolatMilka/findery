# `.claude/docs/` — the source of truth

These pages are written so that **findery is understood faster from
the `.md` than from the `.rs`**. Reading them isn't diligence, it's the
shortest path: an hour spent reconstructing from the source what a page
already states is an hour lost, and the reconstruction is usually wrong in a
way the page would have avoided.

**A page that contradicts the code is a bug in one of the two.** We decide
which, and fix that one. We never route around a doc in silence (CONV-037).

## Map

| Page | What it carries |
|------|------------------|
| [`usage/README.md`](usage/README.md) | Running modes, every CLI flag, every tuning knob, and when to reach for it |
| [`overview/pipeline.md`](overview/pipeline.md) | The full path: file on disk → chunk → vector → hit cited by the agent |
| [`overview/layers.md`](overview/layers.md) | The layer law, the crates, what each is allowed to know |
| [`chunk/README.md`](chunk/README.md) | Rust and Markdown chunking, lexical scanner, windows, merging |
| [`embed/README.md`](embed/README.md) | Models, Qwen3 instruction, local and remote providers, cache recipe |
| [`store/README.md`](store/README.md) | SQLite bucket, vector backends, **the measurements that justify the choice**, export |
| [`index/README.md`](index/README.md) | Incremental sync (the three gates), hybrid search, jobs, watch |
| [`server/README.md`](server/README.md) | HTTP API reference |
| [`mcp/README.md`](mcp/README.md) | MCP tools, wiring into Claude Code, token budget |

**Open** work lives in [`../roadmap/README.md`](../roadmap/README.md) — not
here. Fixing a roadmap entry means deleting it (CONV-038).

## The invariants we don't renegotiate

1. **L0 names, the layers execute.** A type that crosses a boundary is
   defined in `findery-core`, exactly once.
2. **Dependencies go down**, data comes back up through `Result`.
   `tools/watchdogs/check_layers.sh` verifies it.
3. **A chunker is pure.** All of incremental sync rests on
   `chunk(text) == chunk(text)`, forever.
4. **The vector is content-addressed.** Key `(recipe, content_hash)`, never
   the location. Moving a function costs nothing.
5. **The bucket is the durable truth**, the vector store is
   reconstructible. That's what makes the backend swappable.
6. **A performance claim is a reproducible number**
   (`make bench`), otherwise it's gossip.
