# Packaging

The user-facing release unit is one `slop` binary. A `v*` tag builds and tests
four archives:

- Linux x86-64
- Linux ARM64
- macOS x86-64
- macOS ARM64

Each archive includes the binary, README, and MIT license; the release also
contains a separate SHA-256 checksum. GitHub emits a Sigstore-backed
build-provenance attestation for each archive.
The binary embeds the installable slop skill from `crates/slop/assets`, so it has
no runtime dependency on the source checkout.

The SCIP language indexers remain explicit runtime dependencies. JavaScript and
Python indexing uses exact npm package versions declared in `index.rs`; Rust
indexing uses the installed `rust-analyzer` toolchain component.

Crates.io distribution uses package `slop-cli` (the installed binary remains
`slop`). The workspace is published in dependency waves because crates.io must
index each internal dependency before dependent packages can verify:

1. `slop-graph`, `slop-llm`, `slop-parse`, `slop-resolve`
2. `slop-analyze`
3. `slop-lsp`, `slop-mcp`, `slop-proxy`, `slop-tui`
4. `slop-cli`

Run `scripts/publish-crates.sh 0.1.0` to build and fully verify the first-wave
archives, then inspect the dependent crates' file lists. Cargo cannot verify
dependent archives against crates.io until earlier waves have been published.
Each actual wave builds the tarball and performs Cargo's full registry
verification before upload. The execute mode requires a clean worktree and an
exact `v0.1.0` tag at HEAD. Actual publishing is a manual GitHub Actions
workflow: select the `v0.1.0` tag as its ref and supply `0.1.0` as its version
input. The workflow is protected by the `crates-io` environment and requires
its `CARGO_REGISTRY_TOKEN` secret. The script is idempotent across partial
releases and waits for registry indexing between waves.

Local verification:

```sh
cargo test --workspace --locked
cargo build --release --locked -p slop-cli
bash scripts/publish-crates.sh 0.1.0
```
