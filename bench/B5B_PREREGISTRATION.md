# B5b pre-registration: does a vocabulary-primed sketch bind better than the envelope?

Written and committed before any B5b generation or scoring. Follows B5
(`bench/B5_PREREGISTRATION.md`), whose G1 failed: blind sketches name about
2 repository helpers each, too few to bind. Changes after the first
generation are listed at the end with their reason, never made silently.
The gate below is fixed by this commit.

## Question

If the model sees only the names the repository already has near the target
(no signatures, no bodies), does binding its sketch surface the helpers the
real implementation calls better than the envelope, and better than ranking
that same vocabulary without a sketch?

## Tasks

B5's 96 scored tasks, unchanged (`prep2/tasks.jsonl`, sha256 `609ae896…`):
flask 25 and httpx 25 (R8's archived tasks, reproduced exactly) and vigil 46
(R8's ids pinned at f61607df, B5 change 3). vigil is public; both models
predate it. The four R8 vigil targets that do not reproduce at f61607df have
no checkout to bind against, so they are not generated: 96 tasks x 2 models
= 192 generations, not 200. Gold, gold_cross, envelope and file lists are
B5's, from the same gutted copies.

## Vocabulary (computed from the gutted copy, before generation)

- Candidates: B5's catalog (functions and classes outside tests that resolve
  to a def), in the target's file or in a file directly in the target's
  directory (not subdirectories).
- Name shown: the entity id after the module, `::` written as `.`: functions
  and classes by their simple name, methods as `Class.method`. Nested
  functions (parent is a function) are local and excluded.
- Excluded: the target itself and anything under it (whatever the gutted
  body defined is gone from the copy, and ids under the target are dropped).
- Names are deduplicated. Cap 400 tokens (chars / 4 + 1) over the joined
  list (`, `-separated): same-file names alphabetically, then same-directory
  names alphabetically, each kept while the list still fits, stopping at the
  first that does not. The kept names are shown in one alphabetical list
  (no locality or ranking order leaks).
- No names at all: the block is still shown, with `(none)`.

## Prompt

R8's `none` prompt verbatim, with the block inserted where R8 inserts its
context blocks (between the header and the instruction):

    Names that already exist in this repository near this function:
    a, b, C.m, ...

The instruction is R8's `none` instruction ("Implement the body of the
function below..."), not B5's sketch instruction, which did worse.

## Generation

qwen3:8b and llama3.1:8b on matt-pc (Ollama 0.34.3), R8's settings:
temperature 0, seed 0, 8,192-token context, at most 768 output tokens,
reasoning off, one sample per (task, model). Extraction is R8's; unparseable
sketches bind to nothing and score zero.

## Arms

| arm | input | ranking |
| --- | --- | --- |
| bind | the primed sketch | binder v0, unchanged from B5 (full catalog, not limited to the vocabulary) |
| envelope | the target | B5's envelope (adaptive default, per budget) |
| file | target's file | B5's same-file list by line distance |
| vocab-bm25 (control) | target signature and docstring | BM25 (k1 1.2, b 0.75) over the kept vocabulary's entities, each entity's document being the word tokens of its shown name; query is the stub's words without the `raise NotImplementedError` line; positive scores only, ties by entity id |

bm25 and bind+file are computed as in B5 and reported, exploratory. Costs,
budgets (1k, 2k, 4k) and packing are B5's.

## Metrics

B5's: recall@5 (primary), precision@5, cross-file recall@5, recall at 1k,
2k, 4k, bind latency. Added:

- mean candidate calls per parsed sketch (B5's binder count; B5 `none` on
  flask + httpx was 2.3), and mean calls that resolve to a catalog entity;
- share of candidate calls in four buckets: names a vocabulary entry (simple
  name equals the last segment of a shown name), another repository entity
  (exact or normalised name match, not in the vocabulary), a name the
  target's file imports, or invented (none of these).

## Gate G1b

Paired task-cluster bootstrap, 1,000 rounds, seed 20260923, as B5. G1b
passes only if all hold on the pooled set and on vigil alone:

- bind recall@5 minus envelope recall@5 at least +10 points, paired 95%
  interval excluding 0;
- bind precision@5 at least 30%;
- bind recall@5 minus vocab-bm25 recall@5 above 0, paired 95% interval
  excluding 0.

Outcome stated as pass, fail, or mixed, without softening. No change to the
vocabulary, prompt, binder or gate after seeing results; any fitted variant
is exploratory and labelled so.

## Known limits, declared now

- The vocabulary leaks location: a gold helper in the same file or directory
  is named in the prompt, so bind gains mostly where the file arm already
  wins; the vocab-bm25 control is there to separate "the sketch adds
  something" from "the vocabulary alone does it".
- Gold helpers in other directories are never in the vocabulary; cross-file
  recall can still come from names the model invents or remembers.
- Two 7-8B models, one sample each; flask and httpx may be memorised.

## Changes after the first generation

1. **Crash fix only (after generation, before any result was seen).** The
   first score run crashed before printing anything: the call bucket named
   `repo` overwrote the record's repository field. The bucket is renamed
   `other`; definitions and metrics are unchanged.
