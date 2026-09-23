# R8 pre-registration: does the context an agent sees change whether it reuses code?

Written and committed before any generation. Changes after the first
generation are listed at the end with their reason, never made silently.

## Question

When a model implements a function, does slop's context envelope make it call
the repository's existing helpers (the ones the real implementation calls)
more often than the alternatives, without more invented calls?

## Tasks

- Repositories: vigil (private: no model can have memorised it; primary),
  flask and httpx (public: reported separately, possibly memorised).
- Targets: Python functions outside tests, body 5–60 lines, whose index `Calls`
  edges reach 1–10 functions or classes defined in the repository. At most two
  targets per file; no target is a callee of another. Seeded sample
  (20260923): 50 from vigil, 25 each from flask and httpx.
- Each repository is copied, every target is gutted to its signature, its
  docstring and `raise NotImplementedError`, and the copy is reindexed. The
  targets stay in the graph with their callers, without outgoing edges or body
  text, so no arm can read the answer.

## Arms (context beyond the stub; all get the file's imports)

| arm | context |
| --- | --- |
| none | nothing |
| file | the target's (gutted) file, truncated to 4,000 tokens around the stub |
| bm25 | Python skeletons (signature + docstring) ranked by BM25 against the stub, packed to 4,000 tokens |
| slop | the envelope for the target (adaptive default, 4,000-token cap), target item excluded |

Tokens are chars / 4 throughout.

## Models

qwen2.5-coder:7b and llama3.1:8b (Ollama 0.34.3, RTX 2080), temperature 0,
seed 0, 8,192-token context, at most 768 output tokens. One sample per
(task, arm, model).

## Metrics

- **Primary: helper reuse.** The share of the target's repository-internal
  callees (from the original index) whose name the generated function calls,
  bare or as an attribute.
- **Secondary: invented calls.** Whether the generated function makes a bare
  call to a name that is not a repository function or class, a builtin, a
  name imported or defined at the top of the file, or bound inside the
  generated function.
- **Also reported:** unparseable responses per arm (scored as zero reuse and
  excluded from invented calls), prompt tokens per arm.

## Hypotheses and decision rule

Paired per-task differences, bootstrap over tasks (1,000 rounds, seed
20260923), pooled over both models and all repositories unless stated.

- **H1:** slop reuse > bm25 reuse.
- **H2:** slop reuse > file reuse.
- **H3:** slop's invented-call rate is no more than 5 points above file's.
- A hypothesis is supported if the 95% interval of the difference lies above 0
  (H1, H2) or wholly below +5 points (H3). vigil alone is reported for each;
  a pooled result that vigil alone contradicts is reported as contradicted.
- Nulls and reversals are reported with the same prominence as support.

## Known limits, declared now

Name matching can credit a call to a different function with the same name.
A model can reach the right helper by a path the original did not use, which
counts as a miss. Two models on one GPU are not "any model". This measures
reuse in a single generation, not a full agent loop with tools.

## Changes after the first generation

None yet.
