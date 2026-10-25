# PRD P5: PyPI binary wheel

Roadmap: `docs/roadmap.md`, section "Re-plan after G1". Runs in parallel with
P3 on its own worktree (`p5-wheel`).

## Problem

Installing slop today means a Rust toolchain and `cargo install`. The plugin
(P3) needs a zero-setup path: `uvx --from slop-cli slop`. The name `slop` is
taken on PyPI; `slop-cli` is free and matches the crate's package name.

## Outcome

`uv tool install slop-cli` or `uvx --from slop-cli slop` installs a prebuilt
`slop` binary on macOS (arm64, x86-64) and Linux (x86-64, arm64, manylinux).
Wheels are built by the existing tag-triggered release workflow, and the
version comes from one source.

## Scope

- **`pyproject.toml` at the root:**
  - build backend maturin, `bindings = "bin"`;
  - `manifest-path = "crates/slop/Cargo.toml"`;
  - `[project] name = "slop-cli"`, version dynamic from Cargo;
  - MIT license, `readme = "README.md"`;
  - `requires-python = ">=3.8"`, since it is a binary-only wheel.
- **`.github/workflows/release.yml`:**
  - add a wheels job using `PyO3/maturin-action` for the four targets
    (manylinux for Linux), plus an sdist;
  - add a publish job using `pypa/gh-action-pypi-publish` with trusted
    publishing (`id-token: write`), gated on the `pypi` environment;
  - keep the existing tarball archives and attestations; also attest the
    wheels.
- **Version check:** a CI step that fails when the wheel version differs from
  the workspace version.
- **`docs/packaging.md`:**
  - PyPI is the primary install path; GitHub archives are kept; crates.io is
    deferred;
  - the one-time manual steps for the user: create the PyPI project and add
    this repo's `release.yml` as a trusted publisher with environment `pypi`.
- **README:** in the Install section only (P3 owns the rest of the README),
  `uv tool install slop-cli` first, `cargo install` second, plus a note that
  indexing still needs `npx` (Python and TypeScript) or `rust-analyzer`.

## Out of scope

- Publishing to PyPI or TestPyPI, tagging, or pushing. That is outward-facing
  and the user does it.
- Fast mode (indexing without SCIP). A separate PRD.
- Windows wheels.

## Acceptance

- `uvx maturin build --release` on this Mac produces an arm64 macOS wheel.
- That wheel installs into a fresh `uv venv` and `slop --version` prints the
  workspace version.
- `uvx --from <wheel path> slop --help` works.
- The `slop` binary in the wheel has the same size and `--version` as
  `cargo build --release -p slop-cli`.
- `release.yml` passes `actionlint` if available; otherwise say it was not
  linted.
- `cargo test` and `clippy` still pass; nothing in `crates/` changes unless
  the build requires it, and if it does, say why.

## Constraints

Step commits with rationale, no push, no publishing. No emojis; comments at
most 2 lines.
