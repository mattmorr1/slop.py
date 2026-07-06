# slop for VS Code

Surfaces [slop](../../README.md)'s findings as inline diagnostics (squiggles)
in VS Code and Cursor. It's a thin launcher for the `slop lsp` language server —
all the analysis happens in the server, which is editor-agnostic (the same
server backs Neovim, Zed, and JetBrains via their LSP clients).

## Prerequisites

- The `slop` binary on your `PATH` (`cargo install --path crates/slop`), or set
  `slop.path` to its location.
- A SCIP index for the workspace (`slop index .`). Without one, the server
  reports a warning instead of diagnostics — generate it, then run
  **slop: Restart language server**.

## What you get

- Findings appear as diagnostics on save: **Blocking** → Error,
  **Warning** → Warning, **Advisory** → Information. Each message carries the
  fix guidance.
- The index isn't rebuilt automatically. After large refactors, run
  `slop index .` (or `slop check . --reindex`) and restart the server.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `slop.path` | `slop` | Path to the slop binary. |
| `slop.indexPath` | *(empty)* | Path to `index.scip`; empty uses `<workspace>/index.scip`. |

## Developing / running unpacked

```sh
cd editors/vscode
npm install          # pulls vscode-languageclient
code --extensionDevelopmentPath="$PWD" .
```

Then open a workspace that has an `index.scip`.

## Verifiability note

The server (`crate slop-lsp`) is covered by headless tests that drive real
`Content-Length`-framed JSON-RPC (`crates/slop-lsp/tests/lsp.rs`). **This VS
Code shim is not** — exercising it needs a running extension host, which can't
be driven headlessly here. It is deliberately kept to a thin launcher so the
untested surface is minimal; treat `extension.js` as unverified scaffolding
until run in a real editor.
