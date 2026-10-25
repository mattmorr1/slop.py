# Packaging

The user-facing release unit is one `slop` binary. The primary install path is
PyPI, as the binary-only wheel `slop-cli` (the name `slop` is taken):

```sh
uv tool install slop-cli        # or: uvx --from slop-cli slop --help
```

The wheel carries no Python code; `pyproject.toml` uses maturin with
`bindings = "bin"`, so the wheel's `slop` is the same executable that
`cargo build --release -p slop-cli` produces. The version comes from the
workspace `Cargo.toml`; CI fails if the wheel version drifts from it.

A `v*` tag runs `.github/workflows/release.yml`, which builds for four targets:

- Linux x86-64 (manylinux)
- Linux ARM64 (manylinux)
- macOS x86-64
- macOS ARM64

It produces, per target, a wheel and a tarball (binary, README, MIT license,
SHA-256 checksum), plus one sdist. Every wheel, sdist and tarball gets a
Sigstore-backed build-provenance attestation. Tarballs go to the GitHub
release; wheels and the sdist go to PyPI through trusted publishing, after the
tested tarball build succeeds and only if the tag matches the package version.
Windows is not built.

The binary embeds the installable slop skill from `crates/slop/assets`, so it has
no runtime dependency on the source checkout.

The SCIP language indexers remain explicit runtime dependencies. JavaScript and
Python indexing uses exact npm package versions declared in `index.rs`, run via
`npx`; Rust indexing uses the installed `rust-analyzer` toolchain component.

The workspace is two packages: `slop-core` (the `parse`, `graph`, `resolve`
and `analyze` modules) and `slop-cli` (the `slop` binary, with the MCP
adapter as a module). Neither is published to crates.io; that is deferred (see
`docs/roadmap.md`). Install from source with `cargo install --path crates/slop`.

## One-time PyPI setup

The publish job has no token; PyPI trusts this repository's workflow instead.
Before the first tag:

1. On GitHub, create the environment `pypi` (Settings, Environments). Adding
   yourself as a required reviewer makes every PyPI upload need one click.
2. On PyPI, go to Account settings, Publishing, "Add a new pending publisher"
   and enter: PyPI project name `slop-cli`, owner `mattmorr1`, repository
   `slop.py`, workflow `release.yml`, environment `pypi`. The project is
   created by the first successful upload.

Each release after that: set `version` in the root `Cargo.toml` (run
`cargo build` so `Cargo.lock` follows), commit, then push the tag
`v<version>`.

## Local verification

```sh
cargo test --workspace --locked
cargo build --release --locked -p slop-cli
uvx maturin build --release        # wheel in target/wheels/
uvx maturin sdist
```
