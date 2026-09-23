# Orbit deferred research gates

These lanes are deliberately outside v0.1. None is required for codebase-relative
Judgment or deterministic Context from one Repository snapshot. Each starts with
a falsifiable experiment, not production infrastructure.

## Measurement contract

Every experiment records repository size, snapshot latency and peak RSS, hook
p50/p95/p99, context tokens, finding delta, functional-test result, and snapshot
identity. A lane does not advance without a comparable baseline.

Current release-build baseline on the 34 KiB tri-language SCIP fixture, measured
as 100 fresh processes on 2026-09-22: prewrite-hook p50 12.921 ms, p95 13.126 ms,
p99 23.112 ms, maximum 25.475 ms. This validates the sidecar seam only; it is
not evidence for a daemon or for large-repository latency.

## Prospective retrieval calibration, 2026-09-22

Leave-one-out evaluation used existing SCIP indexes without modifying the three
source repositories. Stress Analysis yielded 2 neighbors from 100 functions;
GeoGuessrBot yielded 0 from 95. Vigil yielded 873 neighbors from 5,606
non-test, non-generated functions after source discovery excluded `build/`,
`dist/`, and `htmlcov/`. At the current approximately 0.35 cosine threshold
(`125_000` squared-cosine parts per million), 209 Vigil queries retained a
first neighbor. Raising the threshold to 0.5 cosine would retain 48; 0.6 would
retain 15. These are retrieval counts, not precision estimates.

Source review confirmed the cross-package `init_database` and `delete_secret`
pairs as overlapping implementations. It also found false homes: a TypeScript
graph `visit` matched a Python graph builder through `count`/`get`, and a
database `get_session` matched a default-user initializer through shared setup
calls. Even the highest score (0.76) was a frontend workflow mapper versus a
backend serializer: related schema, different responsibilities. The Vigil SCIP
index was older than source files, so this run cannot establish a publishable
precision claim. Keep the threshold advisory and phrase suggestions as review
prompts. Next: label a balanced sample of proposed writes and nonmatches,
rebuild complete indexes, then choose a threshold using precision and recall.

## Persistent artifact and daemon

First persist one read-only alpha/capability projection keyed by snapshot ID and
compare fresh-process loading with full capture. Continue only if p95 improves by
at least 3x to below 200 ms and the cache stays below twice source-plus-SCIP size.
A daemon is considered only if loading remains at least 30% of hook latency after
that result. It otherwise adds lifecycle, authentication, version-skew, and
crash-recovery work without new analysis leverage.

## Workload telemetry

The first experiment is offline ingestion of one pprof artifact, streaming
frames into entity weights and reporting unmapped samples. Continue only when at
least 90% of relevant samples map, the top-20 hot entities are at least 80%
stable across representative runs, profiler overhead stays below 5%, and the hot
set covers at least 80% of CPU time. No endpoint or continuous collection is
justified before those numbers exist.

## Constrained decoding

Limit the spike to one language, tokenizer, and typed-hole grammar. It may enforce
syntax and in-scope symbols; it cannot enforce effects, which require resolution
and transitive analysis. Continue only if invalid generations fall by at least
50%, task success falls by no more than two percentage points, decode p95 rises
less than 15%, and wall time improves at least 20% versus verifier/resample.

## E-graphs

Require a typed pure IR, proved rewrite laws, explicit exclusion of mutation,
throws, nondeterminism and unknown effects, plus workload-derived costs. Test
only 10–20 straight-line integer/boolean kernels under hard saturation limits.
Advance only if at least 25% beat an optimizing compiler by 20%; productize only
after two representative workloads improve end-to-end by at least 5%, no case
regresses over 1%, saturation p95 stays under one second per function, and peak
memory stays below 256 MB per function.

## RL and RLAIF

No training precedes a versioned trace ledger containing context, patch,
before/after judgment, tests, tokens, and snapshot IDs. First score historical
traces and instruct adversarial agents to maximize the reward. Training requires
at least 10,000 independent trajectories across 20 repositories, human/reward
rank correlation of at least 0.6, exploit rate below 1%, false-positive reward
below 5%, and a best-of-N improvement of at least ten acceptance points without
reducing test pass rate. Deleting tests/code, suppressing findings, indiscriminate
inlining, and modifying the scorer are explicit attacks.

## Order

1. Measurement contract and append-only evaluation traces.
2. Persistent projection spike; daemon only if it still misses latency targets.
3. Typed-hole constrained-decoding spike.
4. Offline workload-profile join.
5. Pure-kernel e-graph spike after workload stability is demonstrated.
6. Reward red-team, then best-of-N reranking, then possible training.
