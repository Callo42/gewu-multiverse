# Native agent programs

A run has two layers:

1. the product's **run supervisor** (`gewu-multiverse --run-agent <run-dir>`), and
2. the real **agent program** it launches: Pi, Codex, DeepSeek Harness or OpenCode.

The supervisor owns the run boundary: the workspace, the saved connection/key, the
instruction, the prior declared steps, the Forgejo MCP bridge, the transcript and the
durable ending. The native program owns its model turns, its tools, its retries, its
compaction and its session. The product does not implement a second model loop,
rewrite the native prompt, or write into a native session store.

## Pinned programs

| id | version | official package | licence | protocols |
| --- | --- | --- | --- | --- |
| `pi` | 1.4.0 | `github.com/earendil-works/pi` release tarball | MIT | Chat Completions, Responses, Anthropic Messages |
| `codex` | 0.164.0 | `github.com/openai/codex` release package | Apache-2.0 | Responses |
| `dsh` | 0.2.1-rc.2 | `@deepseek-ai/dsh` npm package, locked closure | MIT | Chat Completions, Responses, Anthropic Messages |
| `opencode` | 2.1.4 | `@opencode/cli` npm package | MIT | Chat Completions, Responses, Anthropic Messages |

Codex is Responses-only. Every package is installed by the product into
`<data-root>/harnesses/<id>/<version>/`, with `install.json` recording the exact
program, version, official checksum/integrity, licence and entrypoint. An existing
version is left alone. A new stage is unpacked and verified beside it, then renamed
into place; a failed stage is removed and the old install is kept. No program is
downloaded unless the installer flag or the Program screen asks for it.

A fresh installation defaults new agents to Pi 1.4.0. Existing `multiverse.toml`
files without `default_harness` keep the built-in engine, so an upgrade is not a
silent migration. An agent's instruction, workspace, accounts, declared steps,
skills and run records are product-owned files and are not moved when its program
changes.

## Connection and key

Each agent stores its own compatible `base_url`, `protocol`, `model` and optional
Codex sandbox choice. A change is saved for the **next** run. The running run's
`native.json` records the exact program, version, executable, install root, protocol,
base URL, model and Codex sandbox; the transcript's `native_started` record repeats
those facts. The board's Program screen shows the active and next facts separately.

The product never passes the key as an argument. Pi and Codex use a command-backed
credential helper that prints the run-private key file. OpenCode reads the key through
its `{file:...}` option. DeepSeek Harness gets a run-private `.env` with `apiKeyEnv`.
The key file is mode 0600 under the run's private tree and is removed with that tree.

## Forgejo tools

When the agent has its own Forgejo token and at least one clone, the supervisor starts
a minimal stdio MCP server with `--mcp-forgejo <context>`. It exposes exactly the six
existing product tools: list assigned issues, read an issue/PR, comment, list own PRs,
open a PR, update own PR. It reads only that run's token and workspace. Its write
counter is persisted, so restarting the MCP process cannot reset the eight-write
bound. Its per-run ledger is the supervisor's record of what was read/written; outcome
mapping uses that ledger and the native tool events, not the model's prose.

## Confinement

Native starts require a successfully enforced product Landlock ruleset. The product
applies it to the real native executable through `--confined-program`; the native
process and every inherited helper stay under it. If the ruleset is unavailable,
partial, or not fully enforced, the run fails instead of falling back to an
unconfined start. The allow-list includes the workspace, the native program's private
home/config/tmp, the installed program, the exact product helper and the run's
askpass file. It does not include the user's real home, other agents' workspaces, the
product's state/credential roots, or global native stores.

Codex has its own sandbox. Its default remains native. An agent may opt in, by a
second explicit keypress that says the removed layer is Codex's own sandbox and only
the product Landlock boundary remains, to `--sandbox danger-full-access`. The choice
is stored per agent, appears in the immutable run record and is never selected
automatically after an error.

## Current evidence and limits

The candidate's live tests use an isolated bundled Forgejo and a loopback
`synthetic-model` endpoint, never a paid provider.

- Pi, Codex and DeepSeek Harness each completed: real native start under Landlock,
  a native shell write, a model-driven call through the product MCP server to read an
  issue, `git push`, open a pull request as the agent, and comment as the agent.
- OpenCode completed: real native start under Landlock and its own `shell` tool
  wrote a file. OpenCode 2.1.4 exposes MCP tools through its Code Mode catalog; the
  candidate configures the server and records its connection, but a model-driven
  OpenCode MCP call is **not claimed** by this candidate. This remains a gap against
  the full four-program MCP acceptance.
