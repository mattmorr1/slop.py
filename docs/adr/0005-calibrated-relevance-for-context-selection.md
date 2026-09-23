# Context selection is scored by a calibrated relevance model

The context envelope chooses which entities an agent sees, as skeletons, when
it edits one. It used an additive score (graph distance, adjacency, effect
overlap, exemplar bonus) with hand-set constants. B4 (`bench/context_bench.py`)
scores selection against co-change history: when a commit changed several
functions, each needed the others in view. On that measure the additive scorer
recovered 51.4% of co-changed functions at a 4k-token budget, below BM25
(59.3%) and file proximity (57.0%). Two causes: unit weights reward hub
entities, and uncalibrated scores let a ratio greedy chase cheap class stubs.

**Decision.** Each candidate gets `P(co-change | features)` from a logistic
model over twelve features: bias, one-hot graph distance 1–4, same file, same
directory, log line gap within a file, same container, shared effect, is-class,
and log BM25 of the candidate's source against the target's. Candidates are
the graph neighbourhood (≤ 4 hops) plus the target's directory. Selection packs
by probability per token, which is the greedy for expected recall under a
budget. The shipped default is the pooled fit over vigil, requests, flask and
httpx; a repository may replace it with its own fit in `.slop/relevance.json`,
whose bytes join the snapshot's cache key and identity. A malformed or
reordered model file is an error, not a silent fallback.

**Why a fitted model rather than better constants.** The weights say what
matters, and they are stable across held-out folds (same_file +5.9 to +8.2,
same_dir +2.0 to +4.2, d1 +0.5 to +1.3, lexical +0.4 to +1.1, is_class about
−6). Locality dominates graph edges: two of the three strongest signals are not
in the call graph at all, which is why the candidate pool includes the
directory. Every selected item carries its per-feature logit contributions, so
the choice stays explainable.

**Determinism.** Probabilities are rounded to parts per million and ties broken
by entity id, so packing is identical wherever float math differs in its last
bit. BM25's average document length uses an integer total, since a float sum in
hash-map order differs per process.

**Evidence** (run 7dc5255, same 1,326 tasks as the before run d54cc1f, recall
at 4k tokens, commit-cluster bootstrap 95% CI):

| arm | pooled | vigil | requests | flask | httpx |
| --- | ---: | ---: | ---: | ---: | ---: |
| additive scorer (before) | 51.4% | | | | |
| **shipped selector, out of sample** | **66.2%** [62–70] | 74.1% | 70.0% | 73.3% | 57.7% |
| Python reference, out of sample | 68.6% [65–72] | 76.8% | 74.3% | 75.9% | 59.5% |
| BM25 | 59.3% [55–63] | 37.3% | 71.9% | 65.9% | 59.6% |
| file proximity | 57.0% [53–61] | 69.5% | 60.1% | 63.0% | 47.9% |

"Out of sample" means each repository is scored with the model fit on the
other three. The `slop-coverage` arm in the same run (70.7%) uses the pooled
default, which saw every repository, and is in-sample: it is not the headline.

**Train/serve parity.** Rust and the benchmark agree on all 21,396 compared
probabilities to the ppm, pinned by a golden test. The 2.4-point gap to the
Python reference is the edit-zone policy, not the model: the product renders
1-hop neighbours in full (a caller's body matters when editing), which B4's
recall cannot reward. On one httpx target that single body took 40% of a 1k
budget; across targets it averages 3–6% of spent tokens.

**Consequences.** On httpx the model is level with BM25 rather than ahead, and
on requests within noise of it; the pooled lead comes from vigil and flask.
Per-repository calibration (roadmap R3) is the next lever.

**Competitors (bench/README.md).** Aider's repo map alone recovers 9.9% at 4k;
Aider as used (target file in chat plus map) reaches 59.3% but spends 8.8k
tokens doing it, and at 16k, inside its budget, 78.0% against 89.2% for this
selector filling the budget. bge-small embeddings trail BM25. The model is
calibrated within a factor of two across bins (ECE 0.0010), overconfident in
the 0.001–0.01 band and the top bin.

**Adaptive default (d473695).** Candidates beyond the edit zone below
p = 0.01 are not shown. It trades recall for precision: at 8k, 95% of the
recall, 20% fewer tokens, 2.5x precision, and no significant lead over BM25
at >= 8k. `min_probability: 0` restores filling the budget.

**Per-repository calibration (`slop calibrate`).** Mines the repository's own
co-change history (any language `slop-parse` reads), fits with a Gaussian
prior centred on the model in use, picks the prior's strength on an inner time
split of the older commits, and validates on the newest 20% by real envelope
recall. It writes `.slop/relevance.json` only if the fit does at least as well
as the model in use there. The prior matters: fit toward zero, vigil's 48
commits lost 2.1 points; shrunk, it stays at the pooled model (strength 10,000),
while httpx, with more history, moves freely (strength 1, +3.5 points, not
significant). The default was fit partly on these repositories, so "model in
use" is in-sample for them.

Co-change is a proxy for what an agent needs, not the need itself; an
end-to-end run (R8) is still required before claiming agents edit better with
this context.
