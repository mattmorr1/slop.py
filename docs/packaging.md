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

The workspace is two packages: `slop-core` (the `parse`, `graph`, `resolve`
and `analyze` modules) and `slop-cli` (the `slop` binary, with the MCP and LSP
adapters as modules). Neither is published to crates.io; that is deferred (see
`docs/roadmap.md`, P5). Install from source with `cargo install --path crates/slop`.

Local verification:

```sh
cargo test --workspace --locked
cargo build --release --locked -p slop-cli
```
