# The write hook may deny, but only on α-equivalence

The `PreToolUse` hook has been warn-only since it was built: it never sets
`permissionDecision`, on the stated grounds that "a false deny costs more than a
missed finding" — recorded in `docs/harness.md`, in a comment at
`harness.rs:255`, and asserted by a test at `harness.rs:555`. We are now
carving out one exception, so the reasoning needs to survive.

**Decision.** The hook may deny a write when the proposed function body is
**α-equivalent** to a body that already exists in the repo. Every other
modularity signal — including the callee-neighborhood match that names the
function new code probably belongs in — stays advisory `additionalContext`.

**Why the split.** The argument for forcing rather than advising is sound: a
duplicate that cannot be introduced needs no detector, and structural forcing
beats prompting. But it is only sound where precision is provable. Measured
today, the neighborhood signal run *retrospectively over complete functions*
(`parallel-implementation`) scored roughly 3 true to 2 false on first cut, and
reached 3/3 only after three rounds of filtering — IDF distinctiveness,
name-as-role, and sibling-methods-of-a-type. Run *prospectively* it must be
worse, because a proposed fragment's callee set is thinner evidence than a
finished function's. A gate at that precision denies a legitimate new function
roughly two times in five, and the failure compounds: an agent that hits a false
deny routes around the tool — renaming, splitting differently, writing it
elsewhere — which yields worse structure than no gate at all.

α-equivalence has no such problem. It is not a prediction about intent; it is an
identity, precision 1.0 by construction. Denying it needs no precision study.

**Dependency: neither existing hash is α-equivalence.** `body_hash` is exact
modulo comments and whitespace — too strict, since any rename defeats it and the
gate becomes trivially evadable. `structural_hash` collapses names *and*
literals ("same shape, renamed variables / different constants") — too loose,
since `timeout=30` and `timeout=60` hash identically and denying on it blocks a
genuinely different function. The gate requires a third hash that collapses
names while preserving literals, keyed on dependency structure rather than token
position so it survives reordering of independent statements. Until that hash
exists, this ADR describes an intent and not a shipped behaviour.

**Consequences.** D9 ("slop is never its own coding agent") is unaffected — a
deny reports a fact and declines a write; it does not edit. But the tool now has
two classes of write-time signal, and the boundary between them is *provability*,
not severity. Any future promotion from warn to deny has to clear the same bar:
an identity or a measured precision, not an intuition.
