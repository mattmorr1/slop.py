# B6 pre-registration: does the Stop gate fire on code Claude wrote, and is it right?

Written and committed before the gate is run on any corpus commit. Changes
after the first corpus run are listed at the end with their reason, never
made silently. Nothing in the gate, the binary or the rubric is changed after
seeing results; a slop bug found on the way is reported, not fixed.

## Question

How often does the Stop reconcile gate fire on real Claude-written commits,
and what share of its firings are right? Secondary: how good are the advisory
items the `reconcile` MCP tool returns for the same change, and what does the
gate cost in latency?

## Corpus and sampling

Four of the owner's repositories (local, never modified; each replayed in a
full local clone under `/tmp/b6/repos`, plus a fetch of the source's
remote-tracking branches and tags, since a local clone copies only branches):

| repo | Claude commits | one parent | touch a `.py` | sampled |
| --- | ---: | ---: | ---: | ---: |
| claude-skills | 266 | 265 | 122 | 60 |
| gen-ai-privacy | 511 | 507 | 46 | 46 |
| LatexUEA | 36 | 36 | 28 | 28 |
| hiring-agent | 9 | 9 | 4 | 4 |

- A Claude commit carries a `Co-Authored-By: Claude` trailer
  (`git log --all -i --grep`), over every ref in the clone.
- Unit: a Claude commit with exactly one parent whose diff against the parent
  names at least one `.py` file (added, modified or deleted; no rename
  detection). 138 units.
- More than 60 units in a repo: `random.Random(20261025).sample` of the
  sha-sorted list, capped at 60. Only claude-skills is sampled.

## Procedure (`bench/b6_replay.py run`)

Binary: `cargo build --release -p slop-cli` at the commit that adds this file's
parent (`9dd2d80`, merge p3-plugin); sha and dirty flag (of `crates/`) are in
the manifest. Per unit, sequentially:

1. `git checkout -f --detach <parent>`, `git clean -ffdx`; clear the prewrite
   sidecar (each run owns its `TMPDIR`, which `std::env::temp_dir` honours).
2. `slop index <repo>` (timeout 1200 s). A failure is recorded with its error
   and the unit counted unscorable, never dropped.
3. Policy: a committed `slop.toml` at the parent is used as is. Otherwise
   `slop init` is run and its proposals recorded; `slop init` prints them as
   "proposed policy (confirm before enforcing)" and never marks a channel
   confirmed, so no proposed channel is written or used. Consequence, stated
   now: in a repo without a committed `slop.toml`, `infra-bypass` cannot fire.
4. `slop hook session-start` with a fresh session id (baseline + sidecar).
5. `git checkout --detach <commit>` (the commit's tree, as if Claude had just
   written it in that session).
6. `slop hook stop` (payload as `tests/fixtures/hook_payloads/stop.json`),
   wall clock including process start. Hook stderr and every `error` line in
   `.slop/hooks.jsonl` are recorded: a fail-open is an error, not an allow.
7. `reconcile` over MCP stdio (`slop mcp <repo>`, `session_id` = the unit's):
   provable findings with file:line, advisory items (the tool returns at most
   6 per call), the detector count, latency.
8. Consistency: the Stop reason's ids must be a subset of reconcile's provable
   ids, `min(8, n)` of them; a mismatch is a recorded error.
9. "Fixed later" (weak, automatic): for each finding and advisory item, the
   flagged line's text at the commit is looked up in the same file at the tip
   of the branch that contains the commit: `line-present`, `line-changed`,
   `file-removed`, or `no-later-commit`.

A unit is unscorable if indexing fails, a hook exits non-zero or logs an
error, reconcile fails, or step 8 disagrees. Unscorable units are counted per
repo with their cause.

Ledger: one JSON line appended to `bench/results/replay.jsonl` with the
manifest (slop sha, dirty flag, seed, repos and HEAD shas, counts, platform,
runtime) and per-unit results. No source text is stored: ids, file paths,
rules, line numbers, counts, labels and one-line reasons only.

## Metrics (`bench/b6_replay.py score`)

- **Block rate**: units whose Stop decision is `block`, over scorable units.
- **Blocks by rule**: `duplicate`, `missing`, `infra-bypass`.
- **Block precision**: TRUE / (TRUE + USELESS + FALSE) over every provable
  finding (a finding is one id in one unit). UNSURE is reported separately and
  excluded from the denominator; it is also reported as if FALSE.
- **Advisory precision**: same, over a seeded sample of up to 40 advisory
  items (`random.Random(20261025).sample` over all items sorted by id).
- **Latency**: Stop p50 and p95 per repo; SessionStart, index and reconcile
  p50 reported.
- "Fixed later" share among TRUE and among non-TRUE labels, as a weak check on
  the labels.

## Labeling rubric

Labels are by the agent that built the harness, reading the code at the unit's
commit (and the existing function a duplicate points to). That is a labeler
grading its own tool: be strict, and use UNSURE whenever not certain.

- **TRUE**: the fact is correct and acting on it would improve the code (call
  the existing function, fix the call, route through the channel).
- **USELESS** (fact true but useless): correct, but acting on it is no
  improvement (an intentional copy across independent packages or skills that
  ship separately, a test fixture, generated code, a script meant to stand
  alone).
- **FALSE**: the fact is wrong (the "missing" name exists at runtime, the
  "equivalent" function behaves differently, the "overlap" is unrelated).
- **UNSURE**: cannot decide from the code.

For advisory items, TRUE means a reviewer would want to look: reusing or
consolidating with the named function would improve the code.
Labels go to `bench/results/b6_labels.jsonl`, one line each:
`{"id", "repo", "rule", "label", "reason"}`, reason one line.

## What results would mean

- Block rate under 2% of units with block precision at or above 80%: the gate
  is safe but rarely earns its keep on this corpus; its value rests on the
  rare catch, and the write-up says so.
- Block precision under 50%: the gate costs more than it gives on
  Claude-written code (each false block costs a turn), which contradicts the
  P3 premise that only provable facts block.
- Most TRUE blocks `missing`: the unresolved-name check is the useful part;
  most `duplicate`: E-sound reuse is.
- Advisory precision under 30%: advice is noise and should stay out of the
  blocking message.
- Stop p95 above 500 ms on any repo: the harness budget in `docs/harness.md`
  is missed on real code.

## Known limits, declared now

- A replayed commit is the final tree of a session, not each turn: a session
  that Claude fixed before committing never shows its intermediate mistakes,
  so the block rate is a lower bound on what the gate would see live.
- One labeler, who built the tool. No second rater.
- Four repositories, one owner; claude-skills dominates the sample.
- Only Python is judged for `duplicate` and `missing`; `infra-bypass` needs a
  committed `slop.toml`, which none of the four repos has at HEAD.
- Before this commit: `slop index` was run once at HEAD of hiring-agent and
  claude-skills to time indexing, and the harness was smoke-tested on a
  synthetic two-commit copy of `tests/fixtures/toy_repo_slopped`. No gate was
  run on any corpus commit.

## Changes after the first corpus run

1. Harness only, after unit 1 of the first run: `reconcile` over MCP failed
   with "an index was rewritten while capturing analysis; retry the command".
   The tool's own first capture queues a background reindex (the server's
   refresher), which rewrites the index while the same call's `check::run`
   captures again. The run was stopped after that unit and discarded; the
   harness now keeps one `slop mcp` process per unit and retries only that
   error (2 s apart, within the 600 s timeout), recording the retry count.
   The background reindex does not rewrite the prewrite sidecar, so the
   provable and advisory lists are unaffected; only the detector list sees
   the reindexed tree. Reported as a slop bug, not fixed.
2. Scoring only, seen at unit 41: step 8 compared id *sets*, but two new
   copies of one function share one `duplicate:<hash>` id, so a correct Stop
   failed the check. `score` recomputes step 8 as a multiset from the stored
   ids; nothing else about that unit changes. The run-time check is fixed the
   same way for later runs.
3. Exploratory, after the primary run: 42 of 46 gen-ai-privacy units failed
   `slop index` because its Rust crates cannot be indexed (rust-analyzer exits
   1 under the pinned 1.90 toolchain, which lacks the component, and also under
   stable). The primary result keeps them unscorable. A second, separately
   reported run (`--allow-partial-index`, repo label `gen-ai-privacy-partial`)
   continues when the Python artifact was written. SessionStart then refuses
   to persist the sidecar from a stale snapshot, so only `missing` can fire;
   that refusal is recorded, not counted as an error. It never enters the
   primary totals.
