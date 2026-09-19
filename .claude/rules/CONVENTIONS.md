# CONVENTIONS.md — the constitution of findery

> This file is law. Code that contradicts it is a bug in the code, or a
> bug in this file — decide which, fix that one, never silently work around it.

**Language**: everything in the repository — code, identifiers, comments,
rustdoc and documentation — is in **English**. Only conversation with the
user is in **French**.

---

## 1. The founding contract — L0 names, the layers execute

| CONV | Rule |
|------|------|
| **CONV-001** | **`findery-core` names, no one else.** Any type that crosses a boundary (HTTP, MCP, CLI, Rust client) is defined in `findery-core::model` or `findery-core::api`. A struct duplicated in a transport is drift waiting to happen. |
| **CONV-002** | **Transports decide nothing.** An HTTP handler, an MCP tool, a CLI subcommand: validate → call ONE `Indexer` method → serialize. Three lines of business logic in a handler is three divergent implementations tomorrow. |
| **CONV-003** | **Orchestration lives in L2.** "What a sync does", "what a search does" have ONE home: `findery-index`. |
| **CONV-004** | **Traits are the seams.** `Embedder`, `VectorStore`, `Chunker` are the exchange points. A method gets added only if EVERY implementation, present and future, can honor it. |

## 2. The layer law

| CONV | Rule |
|------|------|
| **CONV-005** | Dependencies **go down** (L4 → L3 → L2 → L1 → L0). Data **goes up** through return values (`Result`), never through an import. |
| **CONV-006** | `findery-core` (L0) depends on no workspace crate, opens no file and no socket. It describes. |
| **CONV-007** | `findery-client` stays light: `findery-core` + `reqwest`. Never SQLite, never the walker, never the chunkers — it drops into any project. |
| **CONV-008** | `findery-mcp` goes through the HTTP API, never through the engine. The bridge is a wire, not a cache. |
| **CONV-009** | Mechanical truth: `bash tools/watchdogs/check_layers.sh` (R-01…R-04). Green, or the task doesn't exist. |

## 3. SSOT — one source per fact

| CONV | Rule |
|------|------|
| **CONV-010** | The error → HTTP code mapping lives in `Error::http_status()`. A handler that writes `StatusCode::NOT_FOUND` by hand is a divergence. |
| **CONV-011** | Filter semantics live in `CompiledFilter`. A backend can push part of the predicate down to its engine; what it doesn't push, it evaluates with THIS matcher. |
| **CONV-012** | The Qwen3 prompt format lives in `findery_core::prompt`. No one concatenates `"Instruct: "` anywhere else. |
| **CONV-013** | The vector cache key is `(recipe, content_hash)` and nothing else. `recipe = "<model>:<dim>"`, built in `Indexer::recipe()`. |
| **CONV-014** | Thresholds (`max_chars`, `default_k`, `rrf_k`, `batch_size`…) live in `Config`. A magic number in a function is a named constant that got ignored. |

## 4. Types & signatures

| CONV | Rule |
|------|------|
| **CONV-015** | **Every identifier is newtyped**: `ProjectId`, `ChunkId`, `ContentHash`. Never a bare `String` in a signature — three `String`s in a row end up swapped. |
| **CONV-016** | Validation happens at the constructor (`ProjectId::new`), not at the point of use. A `ProjectId` that exists is a valid `ProjectId`. |
| **CONV-017** | Read-only → `&T`. Mutation → `&mut T`, **only within its phase's write window**. A `clone()` with no written justification is an admission of a failed design. |
| **CONV-018** | A public `enum` exposes symmetric `as_str()` and `parse()`, and the round-trip is tested. |

## 5. Errors — never panic in production

| CONV | Rule |
|------|------|
| **CONV-019** | **Zero `.unwrap()`**, zero `panic!`, zero `unreachable!`, zero `todo!` on a production path. Propagate with `?`. |
| **CONV-020** | `.expect("<invariant>")` is tolerated **in tests only**, and the message states the invariant, not the symptom. |
| **CONV-021** | One `Error` variant = one different caller decision. No "misc" variant. |
| **CONV-022** | **A failure reports itself, it doesn't disguise itself.** An unreadable file becomes a `warning` in the `SyncReport`, not a silent `continue`. |
| **CONV-023** | Any input coming from outside (path, glob, FTS query, JSON) is validated **before** the first side effect. `sanitize_rel`, `CompiledFilter::new`, `fts_query`. |

## 6. Data & mutation

| CONV | Rule |
|------|------|
| **CONV-024** | **One writer, one window, then we read.** The chunker doesn't know the project; L2 stamps the identity; transports mutate nothing. |
| **CONV-025** | A chunker is **pure**: same input, same output, no I/O, no clock, no randomness. All of incremental sync rests on this. |
| **CONV-026** | The catalog is the durable truth. The vector store is **reconstructible** from it — that's what makes the backend interchangeable. |
| **CONV-027** | `chunks` and `chunks_fts` are written in **the same transaction**. A full-text index that lags returns hits pointing at text that's gone. |
| **CONV-028** | A vector write is **idempotent on its id**. A re-sync never leaves an orphan. |

## 7. Files, modules, comments

| CONV | Rule |
|------|------|
| **CONV-029** | **≤ 450 lines of code per file**, tests excluded. `#[cfg(test)]` modules live next to the code they lock down — that's a strength of Rust, not a smell, and they don't count. Past the budget, split **by responsibility** (see `findery-core/src/config/`, `findery-store/src/catalog/`). Mechanical truth: `bash tools/watchdogs/check_file_size.sh`. |
| **CONV-030** | `//!` at the top of every module: what it does, and above all **why this way**. Every `pub` has its rustdoc. |
| **CONV-031** | Comments say **INTENT / WHY**, never WHAT. `// increments i` is noise; `// INTENT: ordering the ties keeps every cache above this honest` is information. |
| **CONV-032** | **Zero time markers**: no "new", "refactored", "old system", no date, no orphan `TODO`. What's left to do goes in `.claude/roadmap/`. |

## 8. Tests

| CONV | Rule |
|------|------|
| **CONV-033** | A test names a **behavior**, not a function: `a_second_sync_costs_no_embedding_at_all`, not `test_sync_2`. |
| **CONV-034** | Every unit shipped has its happy path **and** at least one error case. The error case is the one that protects. |
| **CONV-035** | Any performance claim is **measured and reproducible** (`cargo run -p findery-bench --release`). An unmeasured hunch is gossip. |
| **CONV-036** | Tests touch neither the network nor a model: `EmbedderBackend::Mock` and `Catalog::open_in_memory()` exist for that. |

## 9. Documentation & roadmap

| CONV | Rule |
|------|------|
| **CONV-037** | An observable behavior that changes updates the area page `.claude/docs/<area>/README.md` **in the same change**. A doc that lies is worse than no doc. |
| **CONV-038** | `.claude/roadmap/README.md` carries **open** work. Fixing an entry means **deleting** it. |

---

## Exit checklist — 100%, not a buffet

```
□ cargo fmt --all -- --check
□ cargo clippy --workspace --all-targets -- -D warnings
□ cargo test --workspace
□ bash tools/watchdogs/check_layers.sh
□ zero .unwrap()/panic!/unreachable!/todo! outside #[cfg(test)]
□ bash tools/watchdogs/check_file_size.sh   — ≤ 450 lines of code/file
□ //! on every touched module, rustdoc on every pub
□ happy path + ≥ 1 error case per unit shipped
□ .claude/docs/<area>/ page up to date if an observable behavior changed
□ perf claim ⇒ reproducible number
```

Reference commands — on a machine without `make`, `.\tools\tasks.ps1 <task>`
runs the same list under the same names:

```bash
make check          # fmt-check + clippy + test + layers + sizes
make bench          # measures the vector scan
make up             # CPU stack (model + daemon)
findery health          # what the daemon actually serves
finderyd --check   # the config as actually resolved
```
