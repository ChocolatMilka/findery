# Roadmap — OPEN work

This file carries what isn't done. **Fixing an entry means deleting it**
(CONV-038). Nothing historical here: history lives in git.

---

## Open

### R1 — Cross-encoder reranking (`Qwen3-Reranker`)

RRF fuses two lists by rank; it never **re-reads** the candidates. A
cross-encoder sees the pair (query, passage) in the same forward pass and
models their interaction — it's structurally more precise than a bi-encoder.

*Family*: `Qwen3-Reranker-0.6B / 4B / 8B`, 32k of context, **instruction-aware
in the same format as the embedding** — the `QueryTask` plumbing is directly
reusable. Output = P(yes) on the logits of the `yes`/`no` tokens.

*Seam*: a `Reranker` trait in L0, called in `search::search` between the
`get_chunks` and truncation. **No storage impact**: it's pure query-time, so
no re-sync, no cache invalidation — unlike an embedding model change.
~200 lines.

*The real argument isn't quality, it's token budget.* The agent reads the 8
excerpts anyway; moving the right one from rank 5 to rank 1 gains it nothing
by itself. What would gain something is dropping to `k=3` at a constant
success rate — roughly ~2,500 tokens saved per search.

*Measured blockers*:
- **GPU mandatory.** 20 candidates × ~400 tokens = ~8k tokens of prefill per
  search. On GPU: ~150-250 ms. On CPU: tens of seconds. Dead.
- **Service.** The model card documents Sentence Transformers, Transformers
  and vLLM; **not TEI**. It's a causal-LM-based reranker, not a
  classification cross-encoder — so a second runtime, not a second endpoint
  on the one we already have.

*To do BEFORE writing a line* (CONV-035): a set of 30 real questions, and
measure how often the right answer is present in the current top-3. If it's
already ≥ 85%, the reranker buys nothing.

### R2b — Deduplicate `embeddings` and `vectors` on the `sqlite` side

Measured on a real catalog: `embeddings` and `vectors` carry **exactly the
same bytes** (1055 rows, 540 160 B on each side at 128 dims). Every vector is
therefore written to disk twice — once in the content-addressed cache, once
in the project's live set. At 1024 dims and 50k chunks, that's **205 MB
duplicated**.

*Why it's like this*: `vectors` makes `SqliteVectorStore` self-contained — it
carries the payload (path, lang, kind, symbol) the cache doesn't have, and
the cache is indexed by `(recipe, content_hash)`, not by chunk.

*Lead*: `vectors` keeps only `chunk_id` + payload + `content_hash`, and the
matrix fills in by joining on `embeddings` at warm-up. `SqliteVectorStore`
already holds the `Catalog`, so no layer moves.

*To measure before*: the cost of the join on a cold start, against the
205 MB saved. If warm-up goes from 200 ms to 2 s, the trade-off is bad.

### R2 — Quantizing stored vectors (f16 / int8) on the `sqlite` side
The matrix is in `f32`: 819 MB at 200k chunks × 1024 dims. f16 would cut it
in half for near-zero recall cost on normalized vectors.
*Blocker*: a before/after recall number is needed, not a hunch (CONV-035).

### R3 — Context expansion around a hit
Optionally return the N neighboring lines of a chunk, to avoid the follow-up
`Read` the agent does today when the excerpt is cut too short.
*Likely shape*: `SearchRequest::context_lines`.

### R4 — Extensions beyond `.rs` / `.md`
`Lang::Other` already goes through the generic chunker, so `.toml`, `.sql`,
`.py` work — but without syntactic splitting. A chunker per requested
language, behind the same trait.

### R5 — Persistent watch
Watchers live in the server's memory: a restart loses them. Store them in
the catalog and relaunch them at boot.

### R6 — Prometheus metrics
`/metrics`: search latency, embeddings computed vs cached, index size. Today
observability lives in the logs and `SyncReport`.

### R7 — Vector cache purge
`gc_embeddings()` exists but is exposed nowhere (not CLI, not API). Decide on
a policy (by age? by dead recipe?) before exposing it — the cache is
precisely what makes a moved file free.

### R8 — Real load test
`findery-bench` measures the isolated scan core. What's missing is an end-to-end
measurement: time to first-index a 500k-line workspace, CPU versus GPU, and
the search latency curve as the corpus grows.

---

## Ruled out, and why

| Idea | Why not |
|------|--------------|
| Embedded HNSW index (`hnsw_rs`) | measured: 76 s to build 20k vectors against ~1 ms of gain per query. Bad trade-off on a corpus that mutates on every save. Qdrant covers this need. |
| tree-sitter for chunking | brings in a C toolchain for information we throw away. The lexical scanner is enough for item boundaries; the `Chunker` trait remains the seam if an AST becomes necessary. |
| Qdrant gRPC client | the REST API holds up with `reqwest`, already present, and decouples our release cadence from theirs. |
| Server-specific DTOs | `findery-client` and `findery-mcp` wouldn't see them; they'd diverge at the first field added. The vocabulary lives in L0. |
