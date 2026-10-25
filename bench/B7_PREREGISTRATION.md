# B7 pre-registration: does the Stop gate catch known copies in vigil?

Written and committed before any gate run (SessionStart, Stop or `reconcile`)
on a vigil commit. Changes after the first run are listed at the end with
their reason. Nothing in the gate, the binaries, the positives or the rubric
is changed after seeing results. Misses are explained, not fixed.

## Question

B6 measured how often the gate fires. B7 asks the reverse question on
duplication the vigil maintainers later removed by hand: when one of these
copies was written, would Stop have blocked it with a duplicate finding that
points to an existing copy? And how right are its blocks on ordinary vigil
commits? Arm B (docstring-blind equivalence) is an experiment arm, not a
product decision.

## Corpus

- github.com/Vigil-SOC/vigil, a full clone at `/tmp/b7/vigil`. HEAD (`origin/HEAD`)
  is `fbf68f879cf53622d238994628a2f746476c7762`, with 1547 commits.
- History is `origin/HEAD` with rename detection (`git log -M`). A copy that was
  moved (for example `backend/api/*` to `core/integrations/*` or
  `services/api/routers/*`) keeps its identity, so a move does not count as
  introducing it.

## Groups

The copies were found with pickaxe on the def line:
`git log -M --reverse -p -S'def NAME(' origin/HEAD -- '*.py'`, keeping the
`+`/`-` def lines per file (script `/tmp/b7/scripts/trace.pal`). The introducing
commit is the first commit that adds the def line in that file. I read every
copy at its introducing commit to confirm that it does the same job as the
rest of its group.

| group | copy (path when introduced) | introducing commit | notes |
| --- | --- | --- | --- |
| slugify | scripts/create_workflow.py `slugify` | 3e8abfe8 | first; replace spaces and `_` with `-` |
| slugify | backend/services/custom_agent_service.py `slugify` | bcc689f1 | `_SLUG_RE`, fallback "agent" |
| slugify | services/custom_workflow_service.py `_slugify` | 454873c1 | `_SLUG_RE`, `[:60]`, fallback "workflow" |
| slugify | backend/api/llm_providers.py `_slugify` | d82f75e0 | `_SLUG_RE`, fallback "provider" |
| env_bool | daemon/sandbox_submitter.py `_env_bool` | 4ca26842 | first; default=False; tuple ("1","true",...) |
| env_bool | backend/middleware/security_headers.py `_env_bool` | c05e4e27 | tuple ("true","1",...) |
| env_bool | backend/middleware/csrf.py `_env_bool` | 64ea444b | text-identical to security_headers |
| env_bool | backend/services/auth_cookies.py `_env_bool` | 165849f7 | text-identical to security_headers |
| get_secret | backend/api/darktrace_webhook.py `_get_secret` | 561745bb | first; env only |
| get_secret | backend/api/cloudflare_webhooks.py `_get_secret` | 66e09911 | secrets manager, then env |
| verify_signature | backend/api/darktrace_webhook.py `_verify_signature` | 561745bb | first |
| verify_signature | backend/api/cloudflare_webhooks.py `_verify_signature` | 66e09911 | merged guards, no comments |
| parse_json | backend/api/darktrace_webhook.py `_parse_json` | 561745bb | first; local `import json` |
| parse_json | backend/api/cloudflare_webhooks.py `_parse_json` | 66e09911 | module `json`, `e` renamed to `exc` |
| is_dev_mode | backend/api/vstrike.py `_is_dev_mode` | 74f8ad56 | first; `== "true"` |
| is_dev_mode | backend/services/auth_service.py `_is_dev_mode` | 91f40aa6 | `in ("true","1","yes")` |

The task named the webhook helpers as one group. They are three functions, so
they are scored as three groups, and a hit for one must name a copy of that
same function. `_env_bool`, `_verify_signature` and `_parse_json` are gone at
HEAD: 5a6550b3 retired `_env_bool`, and 5bc533b9 replaced the two webhook
modules' helpers with a shared HMAC receiver. They were found in history.

## Positives

A positive is a copy whose introducing commit comes after another member of its
group already existed at that commit's parent (checked with `git grep` at
`<commit>^`). The first copy in each group is not a positive. That leaves
10 positives in 8 commits:

| # | group | file | commit | members at the parent |
| ---: | --- | --- | --- | --- |
| 1 | env_bool | backend/middleware/security_headers.py | c05e4e27 | sandbox_submitter |
| 2 | env_bool | backend/middleware/csrf.py | 64ea444b | sandbox_submitter, security_headers |
| 3 | env_bool | backend/services/auth_cookies.py | 165849f7 | sandbox_submitter, security_headers, csrf |
| 4 | get_secret | backend/api/cloudflare_webhooks.py | 66e09911 | darktrace_webhook |
| 5 | verify_signature | backend/api/cloudflare_webhooks.py | 66e09911 | darktrace_webhook |
| 6 | parse_json | backend/api/cloudflare_webhooks.py | 66e09911 | darktrace_webhook |
| 7 | is_dev_mode | backend/services/auth_service.py | 91f40aa6 | vstrike |
| 8 | slugify | backend/services/custom_agent_service.py | bcc689f1 | create_workflow |
| 9 | slugify | services/custom_workflow_service.py | 454873c1 | create_workflow |
| 10 | slugify | backend/api/llm_providers.py | d82f75e0 | create_workflow, custom_agent_service, custom_workflow_service |

These positives are a recall test of "the gate blocks a copy of an existing
helper". Most pairs differ in literals, defaults or guards, so they are not
E-sound equivalent. A miss for that reason counts as a miss: the decision rule
asks whether the gate catches the copying that happens, not whether it is
sound.

## Precision sample

These are one-parent commits reachable from `origin/HEAD` whose diff names a
`.py` file (`git log --no-renames --name-only`), excluding the 8 positive
commits. There are 926 of them. The sample is
`random.Random(20261025).sample` of the sha-sorted list, 60 commits
(`b6_replay.py b7 --list` prints it).

## Arms

- **A**: `slop` built from branch `gate-fixes` (release). This is B6's binary
  plus the five gate fixes, including the symlink fix below.
- **B**: `slop` built from branch `equiv-nodoc`, which is A plus "E-sound
  equivalence ignores a function's leading docstring".

The branch shas and binary sha256 values go in the manifest.

## Procedure (`bench/b6_replay.py b7`)

This is B6's replay unchanged, run sequentially for each unit (8 positive
commits, then the 60 sampled commits), arm A first:

1. `checkout -f --detach <parent>`, `clean -ffdx`, then clear the prewrite sidecar.
2. Arm A runs `slop index`, and the resulting `.slop/index` is kept. Arm B
   restores that index instead of reindexing. Indexing code is identical in
   both binaries, and stamps are content digests. If arm A's index fails, the
   unit is unscorable in both arms.
3. Policy: vigil has no `slop.toml`, so `infra-bypass` cannot fire (as in B6).
4. Run SessionStart, `checkout --detach <commit>`, then Stop and record the
   reason. Then run `reconcile` over MCP with the same session, recording
   provable findings (fact and action) and the advisory text.
5. Apply B6's unscorable and consistency rules.

The ledger appends one line to `bench/results/replay.jsonl` with
`manifest.bench = "B7"`: both arms (ref, sha, binary sha256), the vigil HEAD,
the seed, the platform and the runtime.

## Metrics (`bench/b6_replay.py b7-score`)

- **Hit** (per positive, per arm) needs all three of these:
  1. Stop blocks.
  2. Its ids include a `duplicate` finding in the positive's file whose fact
     names the positive's function.
  3. The action points to a group member that existed at the parent
     (`` `...<member function>` at <member file>: ``).
- **Advisory hit**: a `reconcile` advisory item for the positive's function and
  file names a group member (same shape or overlapping calls). Note that a
  function that gets a provable duplicate is never also listed as advisory.
- **Recall**: hits divided by 10.
- **Block precision**: TRUE / all labeled block findings. A block finding is a
  provable id that Stop listed, in one unit of that arm. USELESS, FALSE and
  UNSURE all count against. The decision uses every block finding across
  positive and sample units. The sample-only figure is reported beside it.
- **Why missed**: one line for each miss, read from the ledger and the code,
  for example "not E-sound equivalent: literal tuple order differs", "no
  sidecar", or "indexed but the original was not live".

## Labeling rubric

This is B6's rubric. The labeler is the same model that wrote the gate, so it
must be strict and use UNSURE whenever it is not certain.

- **TRUE**: the fact is correct and acting on it would improve the code.
- **USELESS**: the fact is correct but acting on it would not improve the code,
  for example an intentional copy across separately shipped units, a test
  fixture, or a standalone script.
- **FALSE**: the fact is wrong.
- **UNSURE**: the code does not settle it.

Labels go to `bench/results/b7_labels.jsonl` in the form `{"id", "rule",
"label", "reason"}`, where `id` is `<commit12>:<finding id>@<file>:<line>`.
The same finding in both arms gets one label.

## Decision rule

**Keep the gate** only if arm B blocks at least 50% of positives (5 of 10)
**and** arm B's block precision is at least 50%. Otherwise the verdict is
**park the gate**.

Arm A is reported beside it to show what the docstring change bought. It does
not enter the rule.

## Known before the run

- **A symlink bug, found while preparing this (before any gate run).** vigil
  tracks `tools/splunk.py` as a symlink from 2026-03-29 to HEAD. scip-python
  indexes it, but the source walk skips symlinks. So every capture said "1
  indexed document(s) no longer exist", and SessionStart refused to write the
  sidecar. That would have meant no duplicate detection at any unit: a
  guaranteed 0 recall, which measures the symlink and not the gate. I fixed it
  on `gate-fixes` (f4686f2, test
  `an_indexed_symlink_does_not_make_the_snapshot_stale`) before this file was
  committed. `slop index` at 66e09911^ then wrote the sidecar.
- `slop index` on vigil takes about 30 s per parent, with Python and TypeScript
  both indexed.

## Changes after the first run

(none yet)
