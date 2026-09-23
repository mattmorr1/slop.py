# One product is projected from one repository snapshot

**Decision.** Slop is one product with analysis at its center. Understanding,
judgment, deterministic context, repair planning, and delivery adapters project
from one immutable, content-identified repository snapshot. They do not load or
interpret repository state independently.

The snapshot includes the indexed source generation, graph, effect signatures,
parsed facts, policy, and baseline. Findings and context artifacts carry its
identity and freshness. Refresh creates a replacement snapshot; it never mutates
one already in use.

**Why.** The former execution paths could disagree: `check` and `audit`
duplicated finding evaluation, `fix` used raw detector output, the TUI refreshed
findings without refreshing its graph, and context surfaces did not identify the
source generation they described. Packaging those paths together would preserve
inconsistent behavior behind one binary.

The snapshot is a deep module: deleting it would force every CLI, MCP, LSP, TUI,
hook, and proxy adapter to recover freshness, construction order, effect
inference, policy, and provenance itself. Its interface therefore earns its
seam.

**Constraints.** Identical snapshot, locus, policy, and budget must yield
byte-identical deterministic output. Effect signatures are one evidence and
ranking axis; they do not prove semantic equivalence or runtime cost. Only
provable signals may deny writes. Stale context never skeletonizes silently: it
falls back explicitly to verbatim source. Network or LLM evidence remains
optional and advisory.

**Consequences.** The binary retains supported indexed source once per snapshot
to prevent context rendering from mixing filesystem generations. Cold capture
adds a streaming content-hash pass and O(source bytes) retained memory; warm
adapters share the snapshot through `Arc`. A daemon, generic query engine,
content-addressed persistent store, e-graphs, and reinforcement learning remain
out of scope until measurements justify their complexity.
