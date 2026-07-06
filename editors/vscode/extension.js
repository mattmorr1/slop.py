// The VS Code launch shim for slop's language server. All the analysis lives
// in the `slop lsp` server (crate slop-lsp, headless-tested); this file only
// starts that server and points it at the workspace folder. It is the one
// piece of the editor surface that isn't covered by an automated test, because
// it needs a running VS Code extension host — keep it thin on purpose.

const { workspace, window, commands } = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

/** @type {import("vscode-languageclient/node").LanguageClient | undefined} */
let client;

function startClient() {
  const folder = workspace.workspaceFolders && workspace.workspaceFolders[0];
  if (!folder) {
    return; // no folder open — nothing to analyze
  }
  const cfg = workspace.getConfiguration("slop");
  const command = cfg.get("path") || "slop";
  const cwd = folder.uri.fsPath;

  const args = ["lsp", cwd];
  const indexPath = cfg.get("indexPath");
  if (indexPath) {
    args.push("--index", indexPath);
  }

  const serverOptions = {
    run: { command, args, transport: TransportKind.stdio, options: { cwd } },
    debug: { command, args, transport: TransportKind.stdio, options: { cwd } },
  };
  const clientOptions = {
    documentSelector: [
      { scheme: "file", language: "python" },
      { scheme: "file", language: "javascript" },
      { scheme: "file", language: "typescript" },
    ],
  };

  client = new LanguageClient("slop", "slop", serverOptions, clientOptions);
  client.start().catch((err) => {
    window.showErrorMessage(
      `slop: failed to start language server (${command}). Is slop installed and on PATH? ${err}`
    );
  });
}

function activate(context) {
  startClient();
  context.subscriptions.push(
    commands.registerCommand("slop.restart", async () => {
      if (client) {
        await client.stop();
      }
      startClient();
    })
  );
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
