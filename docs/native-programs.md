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
version is left alone. No program is downloaded unless the installer flag or the
Program screen asks for it.

### Installation, cancellation and recovery

One installer publishes one program at a time. A `.install.lock` file beside the
version roots is held with an advisory kernel lock; a second attempt says the
program is already being installed. The lock is not a stage and is never deleted by
another process. A lock, journal, stage or backup that this installer cannot prove
it wrote is left untouched.

The new tree is built in an owned `.stage-<token>` directory with a mode-0600 owner
marker. The progress samples name the phase (`downloading`, `unpacking`, `installing
packages`, `checking`) and give a percentage only while bytes are actually arriving;
a phase change never leaves the previous percentage on screen. On the Program screen
`i` starts the selected program and `i` again cancels it. Cancelling, Ctrl-C, a
signal, or quitting the board stops the download/unpack/npm child, removes only the
owned stage, and keeps the previous usable version. Quit waits briefly for the
install thread to finish; a hard kill leaves the stage and journal for the next open.

After the stage is complete it is verified the same way the published root is read:
the record names the right program, the entrypoint is an executable below the root,
and DeepSeek Harness still has its native terminal and FFI helpers. Only then is the
old root moved to an owned `.old-<token>` and the stage renamed into place. A
durable `.install-journal` records each state. If the process is killed before the
old root moves, the stage is removed and the old root stays. If it is killed between
the two renames, the next engine open restores the old root from the backup before
the first picture. If it is killed after publication, the verified new root stays
and the owned backup is cleaned. If a cancel/rename failure cannot verify that the old
root is restored, the error names the uncertainty and the journal plus owned backup
are kept; recovery also keeps the journal when the version root is foreign,
ambiguous or the backup is missing, and settles only a verified old restore or an
owned readable new publication. Removing a foreign destination is always the
person's action, never the installer's. A stage left by the earlier pre-journal installer
is recognised only by its exact name and package-file contents; a lone complete
legacy backup is restored when the version root is missing. Foreign stages,
backups, symlinked journals and unrecognised locks are preserved.

The one-line installer uses the same rule at its own level: an exclusive
`.gewu-install.lock` directory, a checksum-verified download, a `--version` run of
the new binary before anything is replaced, and an atomic rename over the old
binary. A checksum failure, an invalid archive, a binary that does not run, a
foreign lock or a cancelled program install leaves the installed binary in place.

### DeepSeek Harness: locked graph and helpers

DeepSeek Harness 0.2.1-rc.2 is installed from its vendored lockfile with `npm ci`,
an isolated `HOME`/cache inside the stage, the official npm registry, and no global
Node/npm, shared cache or user npm setting changed. Node.js 24.9 or newer and npm
11.16.0 or newer are required and are checked before a stage is made. npm before
11.16.0 ignores the project `allowScripts` map, so the installer refuses it rather than
running a denied lifecycle script; the check happens before any stage or download.

The lockfile has five packages with install scripts. This product reviews that exact
set: `node-pty@1.2.0-beta.15`, `koffi@3.1.1` and
`@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2` are allowed because their native
helpers are needed; `@google/genai@2.21.0` and `protobufjs@7.6.6` are recorded as
denied no-ops. Any changed lock with a different script set fails before `npm`
starts. `allowScripts` is not a blanket switch, and this installer never passes
`--dangerously-allow-all-scripts`. A configured
`dangerously-allow-all-scripts=true` is refused before npm runs, because it bypasses
the two explicit denials. The preflight asks npm for the install's effective
empty-project context, so a caller project's `.npmrc` cannot mask user/global/
environment settings; npm's boolean spellings (`true`, `1`, `yes`, `on` and their
false forms) are handled, and an unrecognized value is refused rather than guessed.
If npm is configured to skip install scripts, the install
fails with that reason instead of pretending the program is ready. After
`npm ci`, the installer loads `node-pty`'s native module and executes `koffi`'s libc
`getpid` binding; a `spawn-helper`, where the platform ships one, must be executable.
A missing or non-executable helper fails truthfully and the old version is kept.

The published top-level `@deepseek-ai/dsh` tarball is downloaded again and checked
against its official SHA-256 and the lockfile's SHA-512. `install.json` records that
tarball hash, the exact lock SHA-256, the `node-pty` helper hash and the `koffi`
version. It does **not** claim a recursive tree digest: no such official digest is
published, so the record labels the lock and archive provenance instead.

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
