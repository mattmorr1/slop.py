# E-equivalence ships as a normalizer; egglog stays the oracle

ADR 0001 lets a write be denied only on an identity, and names α-equivalence as
the only one available. Measurement showed α catches little: on 1,835 provably
equivalent mutants of 400 real functions it matched 21.6%. A ternary rewritten
as an `if`, a negated test with swapped branches, or a single-use temporary each
defeat it. This ADR adds **E-equivalence**: α-equivalence modulo rewrite laws.

**Decision.** Laws split into two tiers by soundness, not by usefulness. The
**sound** tier holds for every Python value and may back a deny. The **graded**
tier (commutation, comparison flips, De Morgan, `x += y` → `x = x + y`) holds
only for well-behaved types and is advisory. Both tiers are decided by a
terminating normalizer over a small term language. Bounded egglog saturation
implements the same laws behind the `egraph` cargo feature, off by default, as a
differential oracle.

**Why not use e-graphs in production.** The ablation (B3, `bench/README.md`) ran
both engines over the same 3,256 pairs three times:

| run | sound (norm / egglog) | graded (norm / egglog) | what changed |
| --- | ---: | ---: | --- |
| 6358902 | 1814 / 1814 | 1810 / 1814 | baseline |
| cd9af44 | 1814 / 1814 | 1814 / 1814 | graded confluence fixed |
| 1434d74 | 1819 / 1817 | 1819 / 1817 | locals numbered after normalizing |

The sound laws are confluent under a fixed orientation, so equality saturation
proves nothing a normal form misses. It runs at 2.4 ms p50 per pair against the
normalizer's 0.1 ms, and brings 62 crates. The graded tier was not confluent in
run 1: De Morgan pushed `not` inward before the if-flip could fire, which is the
phase-ordering problem e-graphs exist to solve. egglog found it by proving four
pairs the normalizer could not. Orienting each `if` to the smaller of its two
equivalent forms closed the gap. Fixing it exposed a second bug, a rule
ping-pong that overflowed the stack, which a termination argument (every graded
`if` rewrite strictly decreases one total order) then fixed. In run 3 the
normalizer overtakes egglog: it can renumber locals after reaching a normal
form, while a binder-less e-graph compares pre-numbered terms. So the e-graph's
value here was as a test oracle, not as the engine.

**Soundness evidence.** Across all three runs, 0 of 1,310 behaviour-changing
mutants and 0 of 111 type-conditional traps were judged sound-equal; the rule of
three bounds the true rate below 0.23% at 95% confidence on this distribution.
α v1, the token-adjacency binder it replaces, judged 13.1% of behaviour changes
equal (73% of keyword-name changes, every changed default).

**Consequences.** The deny gate ADR 0001 describes may use the E-sound hash;
wiring it into the write hook still requires the same per-language evidence for
JavaScript/TypeScript and Rust, which have no E-lowering yet. f-strings, nested
function bodies and match statements lower to opaque token streams: equal only
when identical, which costs recall and never soundness. Adding a law requires a
soundness argument for all values, a mutator in the benchmark, and a run showing
the false-equivalence count stays at zero. The e-graph *optimisation* lane in
`ideation/DEFERRED_RESEARCH.md` is unaffected: this ADR uses equality saturation
to decide equivalence, not to rewrite code for speed.
