# Edited documents are reparsed at once and reindexed in the background

ADR 0002 identifies each document by content, so an edit makes exactly that
document stale while every other document stays resolved. Stale meant its
entity line ranges came from the old content, so context fell back to the whole
file verbatim until a reindex: about 40 s of scip-python and scip-typescript on
vigil. Partial SCIP reindexing (`--target-only` plus merging one document into
a shard) keeps exact edges but still pays the type checker's start-up, which
does not fit a sub-second budget.

**Decision.** A capture re-derives line ranges for every stale document from
the parser facts it already computes (`overlay`). Functions map to their old
entities by (name, ordinal among same-named functions); other entities shift by
the delta of the nearest mapped function; a function whose name is gone is
excluded, never rendered. Such a document is `Reparsed`: context uses it and
the artifact lists it, because its edges are still the last index's. Deny and
read compression still require `Resolved`. A document the parser cannot anchor
stays `Stale` and keeps the verbatim fallback. A `Refresher` thread per
long-running server (LSP, MCP) then reindexes the stale indexers, coalescing
bursts of saves, and the LSP republishes diagnostics when it lands. It fires
only for edited documents and at most once per snapshot, so an edit a reindex
cannot clear (an indexer failure memo) never loops.

**Evidence.** On a vigil clone, 20 edits to indexed Python files, each followed
by capture and context for a function in the file: p50 403 ms, p95 408 ms
(gate: 1 s), every target rendered from its new lines, none verbatim. Most of
the time is the capture itself, since the content changed. A test on the toy
fixture shifts a file and deletes a method: the target renders correctly, the
deleted method is never shown, and disabling the overlay turns it back to
`Stale`.

**Consequences.** Between an edit and its reindex, context around an edited
document can miss edges the edit added (a new call) and keep ones it removed.
One delta per non-function entity means an edit inside a class before its first
method shifts that class header by the previous anchor's delta until the
reindex lands. A function added by the edit has no entity until then, so
context for it reports `unknown_target`.
