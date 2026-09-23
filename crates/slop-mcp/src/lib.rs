//! MCP tool surface for slop (M4c, D9): additive, read-only tools an
//! existing agent (Claude Code) drives — slop is never its own agent.
//! `validate_change` gates edits, `get_context_envelope` / `query_subgraph`
//! feed the agent the effect-typed context and graph it needs.

use std::path::PathBuf;

pub mod server;
pub mod tools;

pub use server::serve;
pub use tools::ToolCtx;

/// Serve the MCP protocol over stdin/stdout for `repo` until EOF. This is
/// what `slop mcp <repo>` runs; Claude Code launches it per-project.
pub fn serve_stdio(repo: PathBuf, index: Option<PathBuf>) -> anyhow::Result<()> {
    let refresher = slop_analyze::refresh::Refresher::spawn(repo.clone(), index.clone(), || {});
    let ctx = ToolCtx {
        default_repo: repo,
        default_index: index,
        refresher: Some(refresher),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server::serve(&ctx, stdin.lock(), stdout.lock())
}
