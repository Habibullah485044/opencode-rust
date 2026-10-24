# STRATA: level-aligned repo cartridges for coarse-to-fine recall

Design proposal, 2026-10-23. Status: idea plus feasibility plan. Nothing here is built or measured yet.

Sources:
- NCM, "Next-Concept Modeling" (arXiv 2610.14162), with the `HazyResearch/ncm` code
- Cartridges (arXiv 2506.06266), with the `HazyResearch/cartridges` code
- a prior-art sweep done the same day (section 8)

---

## 1. The problem in this codebase

opencode-rust has no repo memory. Each session rebuilds its picture of the code by calling `grep`, `read` and `glob`. The facts it learns are then thrown away by `compact.rs` once the history passes 80% of `agent.context_tokens`, or lost when the session ends.

Two things make that expensive here:

- **The main model's context is small.** The Kaggle backend (`kaggle_setup_cell.py`) serves Qwen3.8-27B through Ollama with `CONTEXT_LENGTH 8192`, and FIX_PLAN.md:82 already flags it. A few file reads use up the window and compaction kicks in early.
- **There is no index of any kind.** No repo map, no embeddings, no symbol table. Context only arrives through tool calls (server.rs `build_chat_messages`, tools.rs).

Neither paper solves this alone:

- **A Cartridge** needs a white-box model, because it is trained KV injected into attention. Ollama, Anthropic and OpenAI cannot accept one. The prior-art sweep found no hosted API that takes a client-supplied KV prefix.
- **NCM** is fast, but it is a ~100M-parameter generator trained per task. It is not a chat model and cannot replace the main LLM.

STRATA puts both on a side path: a small memory co-processor that the main LLM calls as a tool, whatever that main LLM is.

---

## 2. The idea in one paragraph

Train a small NCM generator whose **prefix slot is filled by a cartridge** instead of by encoded prompt text. Split that cartridge into **strata that line up with NCM's levels**: coarse levels read a repo-wide cartridge, middle levels read module cartridges, and fine levels read per-file cartridges. The agent gets a `recall` tool. A recall decodes the answer level by level. After each level the Rust side **checks the partial answer against the working tree**, and it stops early once the cited paths and symbols exist and have stopped changing. Cartridges are refreshed per file from `git diff`. NCM's own coarse codes act as a semantic fingerprint, so cosmetic edits do not trigger retraining.

---

## 3. Four mechanisms, and why each one is new

### 3.1 Concept cartridge: a cartridge in NCM's prefix slot

In NCM the target levels use block-diagonal attention: every level sees only the prefix plus itself (`next_concept.py:273-292`). Generation computes the prefix KV once and reuses it for all K levels (`next_concept.py:649-657`, `prefix_kv`). So the prefix is the **only** channel that carries information across levels, and it is already a fixed KV block.

Replace that block with trained tensors `{K_l, V_l}` for each layer `l`, which is a Cartridge. Three consequences follow:

- **No prefill at all.** A Cartridge on an autoregressive model still runs the question and answer through a big model. Here the "context" costs nothing at query time. The question goes in as a few raw prefix tokens after the cartridge, using `raw_token_prefix: true` as in the XSum and WMT configs.
- **Training is cheap.** The generator has 25–100M parameters, and the cartridge for an 8-layer, 512-wide model with p=256 positions is about 2M values (~4 MB fp16). Self-study training per repo is a small job on a Kaggle T4, not an A100 job.
- **The loss changes.** NCM trains on VQ codes, not tokens. The teacher's answer text goes through the frozen NCM tokenizer to get the target codes `r_1..K`. The cartridge is trained with NCM's own per-level cross-entropy (paper eq. 10), plus code corruption. Hard targets are fine, because the teacher can be a black-box API that returns no logprobs.

Prior art: no work conditions an NCM-style or other non-autoregressive generator on an offline-trained per-corpus cartridge. The closest is "Strong Drafts Need Compact Memories" (2608.30252), which compresses the live prefix rather than training an offline corpus memory.

### 3.2 Level-aligned strata: the memory hierarchy matches the generation hierarchy

NCM's appendix H shows that fixing the first 3 levels keeps a story's theme and characters while the later levels change the details. Coarse levels hold the gist and fine levels hold specifics. A repo has the same shape: architecture, then modules, then files.

STRATA gives each level band its own cartridge:

| NCM levels (K=8 example) | Reads cartridge | Holds | Size |
|---|---|---|---|
| 1–2 | `repo` | layout, conventions, which module owns what | p≈128 |
| 3–5 | `module/<dir>` | APIs between files, data flow | p≈64 each |
| 6–8 | `file/<path>` | symbols, signatures, line ranges | p≈16–32 each |

This is a mask change only. Today the prefix part of the attention mask is all-visible. Under STRATA, the queries of level band b see only the cartridge slots for band b, plus the question tokens. Which module and file cartridges get loaded is chosen per query: the repo cartridge always, plus the top few module and file cartridges by key–query match. This follows the finding in arXiv 2508.17032 that cartridge keys behave as retrieval routers.

Why this should hold up where plain composition fails:

- "Cartridges at Scale" (2606.04557) reports that naively mixing separately trained cartridges "collapses performance to near chance". Their fix is distractor mixing during training.
- STRATA keeps that fix: during training, each band sees its own cartridge plus random wrong cartridges of the same band.
- It adds a structural limit: a level can only read one granularity, so a file cartridge can never pollute the coarse plan.
- That this helps is a **hypothesis**. Gate G3 in section 6 tests it.

Prior art: no work aligns a memory hierarchy with a coarse-to-fine generation hierarchy.

### 3.3 Verify-as-you-refine: anytime recall that cannot hallucinate silently

NCM can decode the full answer after **every** level (`VQVAETokenizer.decode_multiscales`). That turns recall into an anytime loop that the agent controls from Rust:

```
for k in 1..=K:
    codes_k   = ncm.step(k)                  # one forward pass, all positions in parallel
    draft_k   = decode(z_hat_k)              # whole answer at resolution k
    claims_k  = parse(draft_k)               # (path, symbol, line-hint) triples
    verified  = claims_k.filter(|c| tree.has(c.path) && grep(c.symbol, c.path))
    if verified == verified_{k-1} && !verified.is_empty() && k >= k_min:
        return Pointers(verified)            # early exit
return Pointers(verified_K) or Miss
```

The answer format is fixed and machine-checkable, for example `path:line | symbol | one-line purpose`. Short targets (L≈64 tokens) suit NCM: answers are set-like and order-free, which is the Sudoku-shaped setting where NCM beat every baseline in the paper.

What the main LLM receives:
- **Only verified pointers**, each with a short snippet that Rust reads from disk. It never sees raw model text.
- A wrong claim is dropped. An empty result returns `Miss`, and the agent falls back to grep and read as it does today.
- Worst case it costs one wasted tool call. It never puts a false fact into context.

Prior art: anytime decoding exists, and verification exists in speculative decoding. Checking intermediate coarse-to-fine drafts against an external oracle (the file tree) to stop early is not in the literature the sweep covered.

### 3.4 Git-diff strata refresh, with NCM codes as a semantic fingerprint

Cartridges have no story for updates. The closest work is iterative test-time compaction (NeurIPS 2026 workshop). STRATA uses the hierarchy to keep updates local:

1. `gitsnap.rs` already snapshots before each run. On a new snapshot, list the changed files.
2. For each changed file, encode its chunks with the frozen NCM tokenizer and keep **levels 1–3 only**. That is a few dozen discrete codes per chunk.
3. Compare them with the codes stored when the file cartridge was trained:
   - **Fine codes changed, coarse codes unchanged** (renames, formatting, small edits): retrain only `file/<path>`, which is seconds to minutes.
   - **Coarse codes changed**: also mark `module/<dir>` as stale. The `repo` cartridge is retrained only when several modules drift, or on a schedule.
4. Until retraining finishes, a stale cartridge is **masked out**, and its slot is filled by a fresh `read` of that file. The memory is never silently out of date.

Prior art: no per-file cartridge refresh keyed to git diffs, and no use of VQ coarse codes as a staleness detector.

---

## 4. Where it plugs into opencode-rust

| Piece | Location | Change |
|---|---|---|
| `recall` tool | tools.rs (next to grep/read) | New tool. Args `{query, k_max?}`, returns verified pointers plus snippets. Read-only, so it needs no approval in permissions.rs. |
| Strata client | new `src/strata.rs` | HTTP client for the strata server, the verification loop from 3.3, and strata bookkeeping in SQLite (new table `strata(path, band, code_fingerprint, trained_at, status)` in db.rs). |
| Cartridge selection | `workspace_for(session_id)` server.rs:1430 | Maps the project root to its cartridge set. |
| Compaction rehydration | compact.rs `compact` | After summarizing, add one line to the summary: "repo facts are available through `recall`". Facts dropped by compaction can then be recovered cheaply instead of re-explored. |
| Refresh trigger | gitsnap.rs | After a snapshot, run the 3.4 fingerprint diff and queue retraining. |
| Teacher data | db.rs `messages` | Harvest (question → location) pairs from past sessions: each `grep`/`read` tool call whose result the model then used. These are free self-study examples labelled by the main model. |
| Server | Kaggle notebook (new cell next to `kaggle_setup_cell.py`) | A small Python service on :11436 behind the same cloudflared tunnel. It runs the NCM generator plus cartridge inference and the nightly training jobs. Inference later moves into Rust (section 5). |

The main LLM stays exactly as it is: Ollama on Kaggle, or any cloud API. STRATA never needs a white-box main model. That is the key design decision, because it sidesteps the fact that no hosted API accepts KV.

---

## 5. Build phases

Each phase ends at a gate. A failed gate stops the project or changes the plan.

**Phase 0: does NCM work on code? (Kaggle, Python only)**
- Train an NCM tokenizer on code text (Rust, Python, TypeScript) with target L=64, K=8.
- **G0:** exact reconstruction of at least 99.5% on held-out code lines. Code is less forgiving than prose: one wrong character breaks it. The paper reached 98.8–99.99% on text, but nothing has been tested on code.

**Phase 1: a concept cartridge on one repo.**
- Pretrain a ~30M generator on generic (question → `path | symbol | purpose`) pairs. Generate these from many public repos, using a teacher that reads the files.
- Then train one flat cartridge (no strata yet) on opencode-rust itself with self-study.
- **G1:** top-1 verified-pointer hit rate against a held-out set of 200 real questions from session history.
  - Compare against BM25 over file chunks and against `grep` on the query's keywords.
  - It must beat both, or there is nothing to sell.

**Phase 2: strata and refresh.**
- **G2:** after a 10-file edit, refreshing only the file cartridges recovers at least 95% of G1's hit rate.
- **G3:** composing the strata does not collapse, judged against CAS's reported failure.
- Plus a timing check against retraining everything.

**Phase 3: agent integration and benchmarks.**
- Add the `recall` tool and run bench/ with it on and off. Measure tokens, steps, wall-clock time and task score, especially on the 8k-context Kaggle model.
- **G4:** fewer input tokens per solved task with no drop in score.

**Phase 4: local inference in Rust.**
- Port the NCM inference path to `candle`: the transformer, codebook lookups, area and linear interpolation, and the learned level-0 up/down convolutions. Cartridges become safetensors files under `.opencode/strata/`.
- A ~30M model running 8 short forward passes fits a CPU-only machine like this one. Speed on this machine is unmeasured; see section 7.

---

## 6. Risks, stated plainly

- **No code NCM exists.** Everything depends on G0 and G1. The paper's models are task-specific, and the smallest released one (Sudoku, 25M) was trained on 89-token boards.
- **Is NCM really needed for recall?** A small autoregressive model plus a cartridge could also emit 64 tokens. What NCM adds is the anytime levels (3.3), the coarse codes used as fingerprints (3.4), and the level structure that the strata are built on (3.2).
  - Phase 1 should also train an autoregressive baseline of the same size with the same cartridge.
  - If it matches NCM on G1 and the early exits save little, keep the cartridge parts and drop NCM. That would be an honest outcome.
- **Self-study quality.** The teacher sees one chunk at a time. Questions that span files depend on the harvested session data (section 4) more than on synthetic data.
- **Kaggle limits.** About 30 GPU-hours a week, and sessions end. Training jobs must checkpoint and resume, and the strata server must handle cold starts. The existing tunnel watchdog pattern applies.
- **Composition is unproven at this scale.** CAS shows naive mixing fails. The band-restricted masks are a hypothesis until G3.

---

## 7. Rough costs (estimates, not measured)

- **Cartridge sizes.** Flat repo cartridge: 8 layers × 2 × 256 × 512 ≈ 2.1M values, ~4 MB fp16. File cartridges at p=24: ~0.4 MB each. A 60-file repo is about 25 MB in all.
- **Phase 1 pretraining.** Order of 0.5–1B target-token positions for a 30M model. At an assumed ~50–100k positions/s on a T4, that is 1.5–6 GPU-hours. **This needs measuring in Phase 0.**
- **Recall latency.** 8 forward passes over at most 64 target positions, plus a cached cartridge prefix.
  - On a T4: milliseconds.
  - On this laptop's i5-2540M, measured 2026-10-23 with the released 25M Sudoku-hard model plus its 15M tokenizer, run through PyTorch CPU in one batch of 32:
    - 32 boards in about 9 minutes, including model load, so roughly 15 s per board;
    - 24 of the 32 boards solved (75%). The paper reports 86.1% over the full test set, so 32 boards is too small a sample to compare.
  - So recall served from this laptop's CPU would be slow. Keep inference on Kaggle until a `candle` port is measured. This also makes Phase 4 a real gate, not a given.

---

## 8. Prior-art map (searched 2026-10-23)

What exists:
- **Cartridges and follow-ups:** CAS (composition at scale; naive mixing collapses), "Learned Structure in Cartridges" (keys act as routers), test-time continual cartridges, Attention Matching compaction, Doc-to-LoRA.
- **Speculative drafting with diffusion drafters:** DFlash, DiffuSpec.
- **Compact-memory drafters:** 2608.30252.
- **Drafting from agent history:** SuffixDecoding.
- **Diffusion for code edits:** Diff Diffusion, 2610.17831.
- **Repo-expert fine-tuning:** SWE-Spot.

What was not found, and what STRATA claims:
1. a non-autoregressive or coarse-to-fine generator conditioned on an offline per-corpus cartridge (3.1)
2. a memory hierarchy aligned with the generation hierarchy (3.2)
3. intermediate drafts checked against an external oracle with early exit, used as a guard against hallucination (3.3)
4. per-file cartridge refresh keyed to git diffs, with VQ codes as staleness fingerprints (3.4)
5. cartridges inside a coding agent that work with a black-box main LLM (section 4)

Caveat: the sweep relied partly on abstracts and secondary summaries for 2026 papers. Before claiming novelty in public, read CAS, 2608.30252 and Diff Diffusion in full.
