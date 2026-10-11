# CLAUDE.md — findery

> Local semantic index for codebases. One warm embedding model, multiple
> projects, multiple clients. Rust API, HTTP API, MCP API.

**Language**: everything in the repository — code, identifiers, comments,
rustdoc and documentation — is in **English**. Only conversation with the
user is in **French**.

---

## 📚 `.claude/docs/` is THE source of truth — before, and after

**Before writing anything**, open `.claude/docs/README.md` then the page for
the target area. These pages are written so the project is understood faster
from the `.md` than from the `.rs`: reading them isn't diligence, it's the
shortest path.

**A page that contradicts the code is a bug in one of the two.** Decide
which, fix that one. Never silently work around a doc.

**After changing an observable behavior**, the area page is updated **in the
same change** (CONV-037). The `.claude/roadmap/` ledger isn't concerned: it
carries **open** work, and fixing an entry **deletes** it.

@import .claude/rules/CONVENTIONS.md

---

## 🏛️ The map — five layers, one contract, three seams

```
 L4   findery-mcp (MCP stdio)      findery-cli (`findery`)        the edges — they speak HTTP
 L3   findery-server (axum)                              validate → 1 call → serialize
 L2   findery-index                                      THE ENGINE: sync, search, jobs, watch
 L1   findery-chunk  findery-embed  findery-store  findery-walk  findery-client
 L0   findery-core                                       THE VOCABULARY — zero I/O, zero internal dep.
```

**The contract**: *L0 names, the layers execute.* Any type that crosses a
boundary is defined once, in `findery-core::model` or `findery-core::api`. A transport
decides nothing.

**The three seams** — the only points where an implementation gets swapped:

| Trait | Implementations | What plugs in |
|-------|-----------------|------------------|
| `Embedder` | `OpenAiCompatEmbedder`, `MockEmbedder` | TEI, llama.cpp, vLLM, Ollama, OpenAI, Voyage… local or remote |
| `VectorStore` | `SqliteVectorStore`, `QdrantStore` | where the vectors go |
| `Chunker` | `RustChunker`, `MarkdownChunker`, `TextChunker` | how we split |

**The layer law**: dependencies go down, data goes up through `Result`.
Mechanical truth: `bash tools/watchdogs/check_layers.sh`.

---

## ⚙️ The seven facts that govern the design

1. **The vector is content-addressed.** Key `(recipe, content_hash)`, never
   the location. Moving a function, renaming a module, re-cloning the repo
   or running a `--full`: zero embeddings. That's the project's central
   economics.
2. **`recipe = "<model>:<dim>"`.** Changing model opens a new space; a bucket
   refuses to mix two recipes — at the **operation** that would mix them, never
   at boot, so the daemon stays up to report the conflict and to delete the
   bucket the message recommends.
3. **A chunker is pure.** All of incremental sync rests on
   `chunk(text) == chunk(text)`, forever.
4. **The bucket is the durable truth**, the vector store is
   reconstructible. That's what makes the backend interchangeable.
5. **Search is hybrid.** Vector for the concept, BM25 for the exact
   identifier, RRF fusion by rank — no calibration between two incomparable
   scales.
6. **The index is fed from two sides.** The daemon walks what it can see; a
   client walks what it cannot and pushes the contents. Same walk, same write
   path, and `Origin` records who fed each file so neither side can prune the
   other's. This is what lets the daemon run in a container, or on another
   machine, with nothing mounted.
7. **Nothing about the model is hardcoded.** Backend, URL, route, auth
   header, name, width: it's all data.

---

## 🚀 Commands

**No `make` on this machine?** `.\tools\tasks.ps1 <task>` is the same list with
the same names (`check`, `test`, `install`, `up-model`…). It is the only way to
run the exit checklist on Windows.

```bash
make check                  # fmt-check + clippy -D warnings + test + layers
make test                   # 281 tests
make bench                  # measures the vector scan (release)
make install                # findery + finderyd + MCP bridge into ~/.cargo/bin
make up-model               # the model alone, in Docker — the daemon runs natively
make run                    # the daemon, on this machine
make up                     # all-Docker variant (requires FINDERY_WORKSPACES)
make up-gpu / up-qdrant / logs / health / down

cd <a project> && findery init && findery sync   # register, then index — init writes .findery/config.toml
findery search "where backoff is handled"   # project read from the manifest, walking up
findery search --scope api "…"              # a named restriction the manifest declares
findery health                  # whether the index answers, and what it serves
findery doctor                  # what is missing, and the command that fixes it
findery bucket ls               # the indexes this server serves, and where each file is
findery config set model …      # settings, validated and written to <data_dir>/config.toml
findery config set server …     # the server address — no FINDERY_SERVER_URL needed
finderyd --check                # the config as actually resolved
```

---

## ⚔️ Rules of engagement — common to the whole squad

1. **Completeness or nothing.** You read 100% of the scope, down to the last
   line. Sampling is a lie; "the rest looks fine" without having read it is
   professional malpractice.
2. **Zero ping-pong.** The repo (docs, neighboring code, tests) answers
   before the user does. You ask a question only if it blocks an
   irreversible decision — otherwise an explicit `> Assumption:`, and you
   move on.
3. **The layer law is symmetric.** Dependencies go DOWN; data goes up
   through return values. An edge that goes up is a dead design.
   `tools/watchdogs/check_layers.sh` (R-01…R-04).
4. **Mutation discipline.** Every piece of data has ONE owner, ONE writer,
   ONE write window, then the whole world reads through `&`. `clone()` with
   no written justification is an admission of a failed design.
5. **Measure, don't assume.** Every performance claim is measured and
   reproduced (`make bench`). An unmeasured hunch is gossip.
6. **The pesticide.** You don't fix a bug: you sterilize its lineage. Every
   fix ships the systemic lock — test, invariant, watchdog — that makes
   recurrence IMPOSSIBLE.

---

## 🧭 Where to put what

| What you're adding | Where |
|-------------------|-----|
| A field visible to a client | `findery-core::model` / `findery-core::api` |
| A language to chunk | `findery-chunk` + one line in `Registry` |
| An embeddings provider | often **zero code**: it's config |
| A vector database | `findery-store`, `VectorStore` impl |
| A sync or ranking rule | `findery-index` — **never** in a handler |
| An HTTP route | `findery-server::routes` + `findery-client` method + MCP tool |
| A tool for Claude Code | `findery-mcp::tools` |

---

## 🚫 Traps already paid for

| ❌ | ✅ | Why |
|---|---|----------|
| A DTO defined in `findery-server` | in `findery-core::api` | otherwise client and MCP diverge at the first field |
| `StatusCode::NOT_FOUND` hardcoded | `Error::http_status()` | two routes, two codes for the same failure |
| A bare `String` in a signature | `ProjectId` / `ChunkId` | three `String`s in a row end up swapped |
| Business logic in a handler | an `Indexer` method | three transports, three behaviors |
| `.unwrap()` outside tests | `?` | a daemon doesn't panic |
| A `continue` on an unreadable file | `warnings` in the `SyncReport` | a failure reports itself, it doesn't disguise itself |
| Hashing the chunk's raw text | hashing `embed_input()` | the path is signal, it must count |
| Writing `chunks` without `chunks_fts` | same transaction | hits pointing at text that's gone |
| `println!` in `findery-mcp` | `tracing` on stderr | stdout belongs to the protocol |
| A recipe conflict raised at boot | raised at the operation that would mix | the boot refusal locks the `bucket rm` its own message recommends |
| Deleting a bucket with a filesystem call | through the daemon | on Windows a SQLite file a live process holds open cannot be removed |
| A project's bucket as a column | it *is* which file holds its row | a column is a second record of the same fact, free to disagree |
| A perf claim with no number | `make bench` | CONV-035 |

---

## 👥 The squad — `.claude/agents/`

| Agent | When |
|-------|------|
| `@planner` | a feature to spec end to end |
| `@architect` | internal layout, placement, complexity |
| `@dev` | implementation |
| `@reviewer` | ruthless review of a diff |
| `@bug-hunter` | proactive sweep for what crashes or corrupts |
| `@debugger` | ONE precise bug, reproduced and sealed |
| `@coherence` | structural audit, placement, layers |
| `@perf` | latency, throughput, memory — measured |
