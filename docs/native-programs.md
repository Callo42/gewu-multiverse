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

| id | version | official package | licence | protocols | resident runtime mode |
| --- | --- | --- | --- | --- | --- |
| `dsh` *(fresh default)* | 0.2.2-rc.5 | `@deepseek-ai/dsh` npm package, locked closure | MIT | Chat Completions, Responses, Anthropic Messages | `dsh --profile sdk` (ephemeral `sessionId` per wake) |
| `pi` | 1.6.1 | `github.com/earendil-works/pi` release tarball | MIT | Chat Completions, Responses, Anthropic Messages | `pi --mode rpc --no-session` (`new_session` per wake) |
| `codex` | 0.172.0 | `github.com/openai/codex` release package | Apache-2.0 | Responses | `codex app-server --listen stdio://` (`ephemeral: true` thread per wake) |
| `opencode` | 2.4.2 | `@opencode/cli` npm package | MIT | Chat Completions, Responses, Anthropic Messages | `opencode serve` (archived session deleted after each wake) |

Codex is Responses-only. Codex 0.172.0 declares MCP tools inside the Responses
`namespace` extension, which a connection that speaks only plain function calling
cannot see. For a Codex run the supervisor opens a second loopback transport in front
of the configured model address: each nested tool is sent as a plain function named
`<namespace>__<tool>`, and a plain function call is restored to the namespace shape
Codex routes. The bridge relays the stream as it arrives; its idle timeout resets on
every byte received, and the run's session timeout is the only total bound. The
program keeps its internal tool identity while the connection sees only the plain
protocol. A one-model catalog built from Codex's own metadata keeps a
known model id on direct function tools rather than Responses Lite or code mode, so
the same bridge covers both. The per-agent model test sends one Forgejo tool in that
flat shape and requires a call back, so a text-only endpoint fails the test before a
run.

Every package is installed by the product into
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
owned readable new publication. A verified old restore means its recorded identity is
back, not that a previously incomplete old install is healthy; a later install can
replace it. Removing a foreign destination is always the person's action, never the
installer's. A stage left by the earlier pre-journal installer
is recognised only by its exact name and package-file contents; a lone complete
legacy backup is restored when the version root is missing. Foreign stages,
backups, symlinked journals and unrecognised locks are preserved.

The one-line installer uses the same rule at its own level: an exclusive
`.gewu-install.lock` directory, a checksum-verified download, a `--version` run of
the new binary before anything is replaced, and an atomic rename over the old
binary. A checksum failure, an invalid archive, a binary that does not run, a
foreign lock or a cancelled program install leaves the installed binary in place.

### DeepSeek Harness: locked graph and helpers

DeepSeek Harness 0.2.2-rc.5 is installed from its vendored lockfile with
`npm ci --ignore-scripts`, followed by an explicit `npm rebuild` restricted solely to
the three reviewed native helper packages (`@deepseek-ai/dsh-subprocess-local`, `koffi`,
`node-pty`), using an isolated `HOME`/cache inside the stage, the official npm registry,
and no global Node/npm, shared cache or user npm setting changed. Node.js 24.9 or newer
and npm 11.16.0 or newer are required and checked before staging.

The lockfile has four packages with install scripts. This product statically reviews that
exact set before npm starts (`node-pty@1.2.0-beta.15`, `koffi@3.1.1`, and
`@deepseek-ai/dsh-subprocess-local@0.2.2-rc.5` are rebuilt explicitly; `protobufjs@7.6.6`
is recorded as denied and never executed because `npm ci` runs with `--ignore-scripts` and
`npm rebuild` omits it). Any changed lock with a different script set fails before `npm`
starts, and this installer never passes `--dangerously-allow-all-scripts` (refusing any
user/global config that enables it). After the selective rebuild, the installer loads
`node-pty`'s native module and executes `koffi`'s libc `getpid` binding; a `spawn-helper`,
where the platform ships one, must be executable. A missing or non-executable helper
fails truthfully and the old version is kept.

The published top-level `@deepseek-ai/dsh` tarball is downloaded again and checked
against its official SHA-256 and the lockfile's SHA-512. `install.json` records that
tarball hash, the exact lock SHA-256, the `node-pty` helper hash and the `koffi`
version. It does **not** claim a recursive tree digest: no such official digest is
published, so the record labels the lock and archive provenance instead.

A fresh installation defaults new agents to DeepSeek Harness 0.2.2-rc.5. Existing `multiverse.toml`
files without `default_harness` keep the built-in engine, so an upgrade is not a
silent migration. An agent's instruction, workspace, accounts, declared steps,
skills and run records are product-owned files and are not moved when its program
changes.

## Per-role resident `HarnessRuntime`

On Landlock-confined hosts, the engine may maintain **one resident `HarnessRuntime` host per role** (`--role-runtime <spec>`), confined inside that exact role's Landlock boundary and never shared across roles. Each wakeup dispatches a fresh ephemeral session on the role's warm runtime—avoiding repeated cold start and MCP registration—while guaranteeing zero cross-round chat history carryover. Idle runtimes park automatically after `runtime_idle_secs` (default `300s`) and shut down whenever the role stops or its configuration changes. If a resident runtime is unavailable, the supervisor falls back transparently to a clean confined one-shot run.

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

When the agent has its own Forgejo token and at least one clone, the supervisor owns a
per-role MCP server. Its state, counter, and mandatory 256-bit authentication cookie live
outside every native-writable root; the native program receives a thin relay over a
private Unix socket verified by both `SO_PEERCRED` and the `0600` cookie. By default it
exposes only the six least-privilege product tools (`list_assigned_issues`,
`read_issue_or_pull`, `comment`, `list_my_pull_requests`, `open_pull_request`,
`update_pull_request`); roles may additionally opt into `triage`, `review` (self-review
blocked), or `maintain` (self-merge and unapproved/stale merges blocked) permissions in
`multiverse.toml`. It reads only that role's token and workspace. The write reservation
is persisted under an exclusive lock before a request can take effect, so a restart,
malformed reply or persistence failure cannot reset or undercount the eight-write bound.
The supervisor ledger is the record of what was read or written; native outcomes require
the matching terminal and work facts, not prose alone.

## Confinement

Native starts require a successfully enforced product Landlock ruleset. The product
applies it to the real native executable through `--confined-program`; the native
process and every inherited helper stay under it. If the ruleset is unavailable,
partial, or not fully enforced, the run fails instead of falling back to an
unconfined start. The allow-list includes the workspace, the native program's private
home/config/tmp and the installed program. The native program receives no Forgejo
askpass file: its Git smart-HTTP requests use a fixed-target loopback transport
owned by the supervisor, which adds the agent's existing credential only toward
the configured Forgejo origin. The allow-list does not include the user's real home, other agents'
workspaces, the product's state/credential roots, or global native stores.

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
- OpenCode completed: real native start under Landlock, a native `shell` write, and
  model-driven calls through its Code Mode catalog to read an issue, push a branch,
  open a pull request and comment as the agent. The MCP target is added to the
  run-private OpenCode server after the warm session, and the product admits it only
  after all six tools register.
