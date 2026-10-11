# The layer law

> Dependencies go **down**. Data comes back **up** through return values
> (`Result`), never through an import. An edge that goes up is a dead
> design. Mechanical truth: `bash tools/watchdogs/check_layers.sh`.

## The stack

```
 L4   findery-mcp            findery-cli              the edges: MCP stdio, `findery`
       │                  │                 they speak HTTP, nothing else
       ▼                  ▼
 L3   findery-server                             HTTP transport (axum)
       │                                    validate → 1 Indexer call → serialize
       ▼
 L2   findery-index                              THE ENGINE
       │                                    sync, search, jobs, watch
       ├──────────┬──────────┬───────────┐
       ▼          ▼          ▼           ▼
 L1   findery-chunk  findery-embed  findery-store    findery-client
       │          │          │           │      (findery-client points up at nothing:
       └──────────┴──────────┴───────────┘       it stands alone)
                       │
                       ▼
 L0   findery-core                               THE VOCABULARY
                                            types, ids, errors, config, traits
                                            no internal dependency, no I/O
```

## What each layer is allowed to know

| Crate | Layer | Knows | Doesn't know |
|-------|--------|------|-------------|
| `findery-core` | L0 | types, contracts, config | that a disk or a network exists |
| `findery-chunk` | L1 | how to chunk text | where the text comes from, what it's for |
| `findery-embed` | L1 | how to talk to an embeddings runtime | what a project or a chunk is |
| `findery-store` | L1 | how to persist and search | what produced the vectors |
| `findery-walk` | L1 | which files belong to a project | who will read them |
| `findery-client` | L1 | the HTTP API | the rest of the daemon |
| `findery-index` | L2 | orchestrating the four | that a transport exists |
| `findery-server` | L3 | HTTP, auth, status codes | how a sync works |
| `findery-mcp`, `findery-cli` | L4 | rendering readable results | that a bucket exists |

## The four rules that are verified

| Rule | Statement | Why |
|-------|--------|----------|
| **R-01** | No edge goes up or crosses at the same level | a cycle of layers is a cycle of responsibilities |
| **R-02** | `findery-core` depends on no workspace crate | L0 must stay importable everywhere, including in an isolated test |
| **R-03** | `findery-client` carries no store, index, chunk, or embed | it can be dropped into any project; it must not pull in SQLite |
| **R-04** | `findery-mcp` doesn't touch the engine | the bridge is a wire, not a second daemon |

## Why this split, and not a single crate

Three concrete reasons, not aesthetics:

1. **The client**. `findery-client` has to be able to land in a third-party
   project without dragging `rusqlite`, `ignore` and `notify` along. Without
   the L0/L1 cut, that's impossible.
2. **Swapping backends**. `VectorStore` has two implementations that share
   no line of code. The day a third one shows up, it only has to honor a
   trait — nothing in `findery-index` moves.
3. **The cost of a bug**. "What does a sync do" has ONE home. When the
   behavior is wrong, there's one file to read, not three transports to
   compare.

## Where to put something new

| What you're adding | Where |
|-------------------|-----|
| A field a client can see | `findery-core::model` or `findery-core::api` |
| A rule about which files count | `findery-walk` — both feeding sides get it |
| A language to chunk | `findery-chunk`, + one line in `Registry` |
| An embeddings provider | `findery-embed` — often **zero code**: it's config |
| A vector database | `findery-store`, impl of `VectorStore`, + a branch in `build_vector_store` |
| A sync or ranking rule | `findery-index` — **never** in a handler |
| An HTTP route | `findery-server::routes` + a `findery-client` method + an MCP tool |
| A tool for Claude Code | `findery-mcp::tools` |

## The trap we already dodged once

The DTOs nearly ended up living in `findery-server`. Result: `findery-client` couldn't
see them, `findery-mcp` would have redefined its own, and the three would have
diverged at the first field added. They live in L0 (`findery-core::api`) —
adding a field breaks everyone's compilation, which is **exactly** the
desired failure mode.
