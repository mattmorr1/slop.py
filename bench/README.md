# Benchmarks

## `compression_bench.py` — token-efficiency A/B

Quantifies the deterministic half of the harness thesis (D11): zoned
graph-distance compression keeps the code an edit is *near* at full fidelity
and skeletonizes the rest. For each repo it picks an edit locus (one file) and
measures the cost of reading other large files two ways — naive (whole file)
vs slop (zoned-compressed) — reporting the reduction (tokens ≈ chars / 4).

```
cargo build --release
python3 bench/compression_bench.py [repo ...]
```

Defaults to `~/Documents/GitHub/{stress-analysis,geoguessrbot,vigil}` (needs a
`<repo>/index.scip`). Vendored dirs (`venv`, `site-packages`, …) are excluded —
slop doesn't index them, so they'd skeletonize 0% and skew the numbers.

### Latest results (2026-07-05, hops=1, top-6 files/repo)

| Repo | orig chars | slop chars | reduction |
| --- | ---: | ---: | ---: |
| stress-analysis | 54,109 | ~18.8k | **−66%** |
| geoguessrbot | 57,003 | ~18.4k | **−68%** |
| vigil (app code) | 533,611 | ~215k | **−60%** |
| **overall** | **644,723** | **257,695** | **−61%** (~161k → ~64k tokens) |

Files near the edit locus keep more of their body by design (e.g. vigil
`database/models.py` −32% — it's imported/called by the edited code), which is
the point: the compression is *relevance-aware*, not blind truncation.

**Integrity:** the compressed view is a read-path artifact — the source files
on disk are never touched. And the view stays *valid Python*: all 23 compressed
files across the three repos parse with `ast.parse` (skeletons preserve their
original indentation, decorators included).

The other half of the thesis — fewer *introduced* slop findings when an agent
edits with steering vs without — needs live agent runs (via `slop proxy --log`
for tokens + `slop gate`/`validate_change` for findings) and isn't captured
here.
