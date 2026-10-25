//! LSP surface for slop: a language server that publishes slop's findings as
//! editor diagnostics. Editor-agnostic (VS Code, Neovim, Zed, JetBrains all
//! speak LSP) and additive — it drives the same `check::run` the CLI and MCP
//! tools use, and never edits. `slop lsp <repo>` launches it over stdio.

use std::path::PathBuf;

pub mod diagnostics;
pub mod server;

pub use server::{serve, LspCtx};

/// Serve the LSP protocol over stdin/stdout for `repo` until the client sends
/// `exit` (or closes the pipe). This is what `slop lsp <repo>` runs; an editor
/// launches it per-workspace.
pub fn serve_stdio(repo: PathBuf, index: Option<PathBuf>) -> anyhow::Result<()> {
    let ctx = LspCtx { repo, index };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server::serve_async(ctx, stdin.lock(), stdout)
}
