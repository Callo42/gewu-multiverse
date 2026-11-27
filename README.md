# gewu-multiverse

[English](README.md) · [简体中文](README.zh-CN.md)

**Traditional agent harnesses define one-off tasks. `gewu-multiverse` defines long-lived roles.**

`gewu-multiverse` gives you a **private internal GitHub** (a bundled, loopback-only Forgejo) paired with a **customizable team of role-defined AI agents**—all in one lightweight binary you run on your own Linux machine.

Each agent has its own Forgejo identity (`gewu-<role>`), its own Landlock-isolated workspace, its own instruction, skills, external MCP servers, and role-scoped credentials. Roles coordinate asynchronously through repositories, issues, pull requests, and reviews—and interact with the outside world by writing and running code inside their own sandbox.

```text
  ┌─────────────────────────────────────────────────────────────────────┐
  │  TUI (gewu-multiverse)  ── auto-attach over headless.sock ──▶       │
  │  CLI (--control/--status)      (q detaches; daemon keeps running)   │
  └────────────────────────────────────────┬────────────────────────────┘
                                           ▼
  ┌─────────────────────────────────────────────────────────────────────┐
  │                 gewu-multiverse --headless (Outer Daemon)           │
  │                                                                     │
  │   ┌─────────────────────────────────────────────────────────────┐   │
  │   │  Bundled Forgejo 17.0.0 (127.0.0.1 only, Shared Truth)      │   │
  │   │  team/* repos · issues · PRs · reviews · HMAC webhooks      │   │
  │   └──────────────────────────────┬──────────────────────────────┘   │
  │                                  │ wake (event / hybrid / periodic) │
  │         ┌────────────────────────┼────────────────────────┐         │
  │         ▼                        ▼                        ▼         │
  │  ┌──────────────┐        ┌──────────────┐        ┌──────────────┐   │
  │  │ role: pm     │        │ role: coder  │        │role: reviewer│   │
  │  │ (Landlock)   │        │ (Landlock)   │        │ (Landlock)   │   │
  │  │ workspace/   │        │ workspace/   │        │ workspace/   │   │
  │  │ skills/ mcp  │        │ skills/ mcp  │        │ skills/ mcp  │   │
  │  │ HarnessPool  │        │ HarnessPool  │        │ HarnessPool  │   │
  │  └──────────────┘        └──────────────┘        └──────────────┘   │
  └─────────────────────────────────────────────────────────────────────┘

  Each wakeup runs a stateless closure inside that role's own sandbox:
    [Trigger] ─▶ [Mechanical <environment-snapshot>] ─▶ [Fresh Ephemeral Session]
              ─▶ [Objective Action Ledger]           ─▶ [Discard Session]
```

## Why role-defined?

Single-agent CLIs are built around one user driving one task in one growing chat thread. As context accumulates, conversations drift, responsibilities blur, and teams collapse into manual copy-paste.

`gewu-multiverse` flips the model:

- **One shared source of truth:** Bundled Forgejo (`127.0.0.1` only) holds all repositories, issues, branches, pull requests, reviews, and audit history. Agents never pass fragile hidden state behind your back.
- **Stateless per-wake closure:** No cross-round chat history carryover. On every wakeup, the supervisor mechanically builds a concise `<environment-snapshot>` (wake reason, local git state, assigned issues, own open PRs, plus any role-selected views or sandboxed `hydrate_cmd` output), runs one **fresh ephemeral session** against the role's `instruction.md`, classifies the outcome from an **objective action ledger** (MCP writes, Git pushes, and native terminal facts—language-agnostic across English and Chinese prompts), and discards the session.
- **Two-layer server architecture:**
  - **Outer daemon (`--headless`):** Runs cleanly under `systemd` or in the background. Opening `gewu-multiverse` interactively automatically detects and attaches to a running daemon over its private Unix socket (`q` detaches without stopping Forgejo or agents; `Q` stops the daemon).
  - **Inner per-role resident `HarnessRuntime`:** Strictly **one Landlock-confined runtime per role** (never shared across roles; every per-role runtime socket, MCP bridge socket, and loopback Git proxy is authenticated with a per-role 256-bit `0600` cookie held outside other roles' Landlock allow-lists), idle-parked automatically after `runtime_idle_secs` (default `300s`). Supports 4 pinned open-source harnesses: **DeepSeek Harness (`dsh` `0.2.2-rc.5`, fresh-install default)**, **Pi (`1.6.1`)**, **Codex (`0.172.0`)**, and **OpenCode (`2.4.2`)**. The legacy `built-in` loop stays frozen for backward compatibility.
- **Switchable wake modes (`periodic` | `event` | `hybrid`):** Fresh installations default to `hybrid` (instant local Forgejo HMAC-SHA256 webhooks with delivery deduplication, debounce, cooldown & anti-echo, plus a 300s fallback check). Roles can opt into pure `event` or fixed `periodic` polling. **Stopped roles (`desired = "stopped"`) are strictly silent: 0 wakes and 0 model calls under every mode.**
- **Tiered internal GitHub permissions:** Every role starts with least privilege (list assigned issues, read issue/PR, comment, list/open/update own PRs, push branches via authenticated loopback Git proxy—no raw Forgejo token in the sandbox). Grant higher team authority per role only when needed:
  - `triage`: list/create/update/assign/label/close/reopen issues;
  - `review`: list PRs, read diffs (stream-bounded), submit formal reviews (**supervisor hard-blocks self-review and verifies live token identity**);
  - `maintain`: merge approved non-own PRs (**supervisor hard-blocks self-merge and requires a current, non-dismissed, non-stale non-author `APPROVED` review matching the PR head SHA with `head_commit_id` pinning and zero unresolved `REQUEST_CHANGES`**) and create private `team/*` repos. Repository deletion and organization admin remain strictly human `owner`-only.
- **Real-world execution inside strict local confinement:** Automatic kernel **Landlock** confines every role's writable filesystem access to its own `workspace/` and exact per-role leaf runtime directories (`home`, `tmp`, `xdg`, and the harness state leaf under a non-writable parent), excluding `$HOME`, saved credentials, daemon cookie, Forgejo data, sibling workspaces, `/run/user/*`, and `/dev/shm`, and enables Landlock `Scope::Signal` + `Scope::AbstractUnixSocket` where supported by the host kernel (Landlock ABI v6+). To connect roles to external systems (cloud APIs, ticketing, docs, data pipelines), attach role-scoped `env`, `0600` external API secrets (`credentials.toml` `[agent_env_secrets.<role>]`, auto-redacted from logs/transcripts), `skills/`, and `mcp.json`.

## Configure a team (Layout A)

Team topology lives in `~/.config/gewu-multiverse/multiverse.toml`; each role's equipment lives under `~/.config/gewu-multiverse/agents/<role>/`:

```text
~/.config/gewu-multiverse/
├── multiverse.toml          # Team topology, wake modes, context & permissions
├── credentials.toml         # Mode 0600: model keys, Forgejo tokens, role external secrets
└── agents/
    ├── pm/
    │   ├── instruction.md   # Role charter & operating procedure
    │   ├── skills/          # Role-specific skills
    │   ├── mcp.json         # Optional role-specific external MCP servers
    │   └── workspace/       # Role's private Git clones (Landlock writable root)
    ├── coder/
    ├── reviewer/
    └── ops/
```

Example `multiverse.toml`:

```toml
terms_acknowledged = true
terms_version = 1
default_harness = "dsh"
wake_mode = "hybrid"          # periodic | event | hybrid
default_context = "self"      # none | self
runtime_idle_secs = 300

[endpoint]
base_url = "https://api.example.com/v1"
protocol = "chat-completions"
model = "your-model-id"
max_turns = 24

[[agents]]
name = "pm"
desired = "started"
harness = "dsh"
wake_mode = "hybrid"
[agents.context]
mode = "self"
views = ["unassigned_issues", "open_pulls"]
[agents.permissions]
triage = true

[[agents]]
name = "coder"
desired = "started"
harness = "codex"
wake_mode = "event"           # wakes instantly on assigned issue / review feedback

[[agents]]
name = "reviewer"
desired = "started"
harness = "dsh"
wake_mode = "event"
[agents.context]
mode = "self"
views = ["open_pulls"]
[agents.permissions]
review = true
maintain = true               # can merge approved non-own PRs; self-review & self-merge hard-blocked

[[agents]]
name = "ops"
desired = "started"
harness = "pi"
wake_mode = "hybrid"
wake_interval_secs = 600
[agents.context]
mode = "self"
hydrate_cmd = "python3 scripts/check_alerts.py --summary"  # runs inside same Landlock sandbox
[agents.env]
SERVICE_ENV = "staging"       # paired with 0600 [agent_env_secrets.ops] in credentials.toml
```

Semantic role names (`pm`, `coder`, `reviewer`, `ops`) work seamlessly alongside classic `agent-1`, `agent-2`, ... names. Every new field is optional.

## Install

One line, no Rust required (Linux `x86_64`, glibc 2.39+):

```sh
curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh

# Offline / air-gapped install from a pre-downloaded release tarball:
sh install.sh --tarball ./gewu-multiverse-*-x86_64-linux.tar.gz [--checksum ./file.sha256] --harness none

# DeepSeek Harness (dsh) is installed by default. Other official programs are explicit:
sh install.sh --harness dsh,codex
sh install.sh --harness pi
sh install.sh --harness none        # no agent program; a new agent still starts as DeepSeek Harness
```

`--harness none` installs no native agent program. A fresh installation still defaults a new agent to DeepSeek Harness, which is not installed until you install it from the Program screen (`p` → `i`) or switch the agent to another installed program or the legacy built-in engine.

The release `.tar.gz` bundles both `gewu-multiverse` (`~/.local/bin/gewu-multiverse`) and the unmodified, GPG-verified `forgejo-17.0.0-linux-amd64` binary (`~/.local/share/gewu-multiverse/`, zero second-stage Forgejo download). Unless `--harness none` is passed, the installer fetches the chosen pinned agent program(s) into `~/.local/share/gewu-multiverse/` and verifies every SHA-256 / npm lock integrity digest. DeepSeek Harness (`dsh 0.2.2-rc.5`) requires Node.js 24.9+ and npm 11.16.0+ and is installed with `npm ci --ignore-scripts` followed by an explicit `npm rebuild` of only the three reviewed native helpers (`@deepseek-ai/dsh-subprocess-local`, `koffi`, `node-pty`).

From source (Rust 1.85+):

```sh
cargo install --git https://github.com/Callo42/gewu-multiverse
```

## Quick start

### Interactive terminal board

```sh
gewu-multiverse
```

1. **First run:** Review and accept the host confinement warnings (`y`), configure your default model endpoint (`t` to test), choose initial agents, and press `⏎` → `e` on each card to write its `instruction.md`. Press `s` to start or stop an agent, `p` to switch or install harnesses, `f` to view Forgejo credentials/URL, and `?` for all keys.
2. **Auto-attach to a running daemon:** If `gewu-multiverse --headless` is already running against the same directories, running `gewu-multiverse` automatically attaches to it as a live console. Pressing `q` detaches cleanly without interrupting Forgejo or running agents; pressing `Q` stops the daemon cleanly.
3. **Scripted preview without Forgejo:** `gewu-multiverse --demo`

### Headless daemon & automation (`systemd` / CI / servers)

```sh
# 1. Configure non-interactively (secrets come from files or stdin, never argv):
gewu-multiverse --configure \
  --accept-terms \
  --add-role coder \
  --agent coder \
  --program dsh \
  --wake-mode hybrid \
  --context-mode self \
  --base-url https://api.example.com/v1 \
  --model your-model-id \
  --api-key-file /run/secrets/model.key \
  --instruction-file ./coder.md

# Optional: grant reviewer permissions and attach a role-scoped external secret:
gewu-multiverse --configure \
  --add-role reviewer \
  --agent reviewer \
  --allow-review \
  --context-view open_pulls \
  --env REVIEW_STRICT=1 \
  --secret-env-file WEBHOOK_TOKEN=/run/secrets/external.token

# 2a. Start standard foreground daemon (systemd compatible):
gewu-multiverse --configure --start-all
gewu-multiverse --headless

# 2b. Warm standby with strictly zero idle LLM calls:
gewu-multiverse --configure --stop-all
gewu-multiverse --headless --allow-idle

# 3. Control live roles without restarting Forgejo (instruction.md is re-read automatically):
gewu-multiverse --control --start-agent coder --stop-on-no-work
gewu-multiverse --control --stop-agent coder
gewu-multiverse --control --stop-all --json

# 4. Read-only health & status (does not acquire the daemon lock):
gewu-multiverse --status
gewu-multiverse --status --json
```

All custom paths can be relocated via `--config-dir <dir>`, `--state-dir <dir>`, and `--data-dir <dir>`.

## Compatibility & upgrading to `v0.2.0`

Upgrading an existing `v0.1.x` installation (including downstream `matrixorg` headless deployments) is **non-breaking**:

- Existing `multiverse.toml` files without `default_harness`, `wake_mode`, or `default_context` automatically preserve `built-in` (or their saved harness), `wake_mode = "periodic"`, `context = "none"`, and least-privilege Forgejo tools.
- `--headless --allow-idle`, `--control`, `--configure`, `--status --json`, and `headless-status.json` remain 100% backward-compatible.
- `desired = "stopped"` remains guaranteed to trigger **0 wakes and 0 model calls** in every wake mode.
- The new defaults (`dsh`, `wake_mode = "hybrid"`, `default_context = "self"`) apply **only to fresh installations** or when you explicitly enable them.

## Security boundaries & what it does not do

- **Not a container or VM:** Agents run as your local OS user. **Landlock** is applied automatically (verified by a live child probe before being claimed) and is compatible with `systemd` `NoNewPrivileges=true` + `ProtectHome=true`. Signal and abstract-Unix-socket scoping (`Scope::Signal`, `Scope::AbstractUnixSocket`) are applied automatically on Linux kernels supporting Landlock ABI v6+; on older Landlock kernels, same-UID signal isolation is not enforced by Landlock, and `/proc` remains readable subject to host `hidepid` / Yama `ptrace_scope` settings (opt-in `GEWU_CONFINEMENT=bwrap` or `unshare` is also available).
- **Strict filesystem & IPC boundary:** Aside from fixed read-only system trees (`/usr`, `/etc`, `/proc`, `/sys`, ...) and safe `/dev/*` device nodes, confined runs can write user data only to their own `workspace/` and exact per-role leaf directories (`home`, `tmp`, `xdg`, and the harness state leaf under a non-writable `<TMPDIR>/gewu-rt-*` parent); Landlock opens every writable grant component-by-component with `O_NOFOLLOW` so symlinks can never redirect confinement to `~/.config/gewu-multiverse`, `credentials.toml`, `headless.sock.cookie`, Forgejo data, `/run/user/*`, `/dev/shm`, your `$HOME`, or sibling agents' workspaces. Every supervisor IPC endpoint (`headless.sock`, `runtime.sock`, `mcp.sock`, loopback `GitProxy`) strictly requires a random 256-bit `0600` cookie or proxy credential. Private host paths outside the allowed system/runtime trees are never opened.
- **No secrets in argv or logs:** Model keys, Forgejo tokens, and role external secrets (`[agent_env_secrets.<role>]`) live in mode-`0600` `credentials.toml`; `mcp.json` forbids `${SECRET}` references in `command`, `args`, and `url` (allowing them only in `env` and HTTP `headers`). Logs, transcripts, and `hydrate_cmd` output scrub raw, JSON-escaped, URL-encoded, and Base64 secret forms.
- **Loopback-only Forgejo:** Bound exclusively to `127.0.0.1`; no external host exposure, no cloud dependency, and no multi-tenant hosting.

## Where things live

| What | Path |
|---|---|
| Binary | `~/.local/bin/gewu-multiverse` |
| Team & role configuration | `~/.config/gewu-multiverse/multiverse.toml` |
| Secrets (mode `0600`) | `~/.config/gewu-multiverse/credentials.toml` |
| Per-role files (`instruction.md`, `skills/`, `mcp.json`, `workspace/`) | `~/.config/gewu-multiverse/agents/<role>/` |
| Run records, logs & transcripts | `~/.local/state/gewu-multiverse/<role>/<run>/` |
| Daemon snapshot & Unix control socket (`0600` cookie) | `~/.local/state/gewu-multiverse/headless-status.json`, `headless.sock`, `headless.sock.cookie` |
| Bundled Forgejo & installed harnesses | `~/.local/share/gewu-multiverse/{forgejo,harnesses}/` |

Uninstall completely:

```sh
rm ~/.local/bin/gewu-multiverse
rm -rf ~/.config/gewu-multiverse ~/.local/state/gewu-multiverse ~/.local/share/gewu-multiverse
```

## Documentation

Detailed engineering contracts live in `docs/`:

- [docs/native-programs.md](docs/native-programs.md) — Pinned harnesses (`dsh`, `pi`, `codex`, `opencode`), per-role resident `HarnessRuntime`, and install verification
- [docs/engine.md](docs/engine.md) — Engine lifecycle, wake scheduling, headless daemon, and recovery
- [docs/agent-execution.md](docs/agent-execution.md) — Confinement, Landlock boundary, run records, and objective outcome ledger
- [docs/forgejo.md](docs/forgejo.md) — Bundled Forgejo provisioning, tiered MCP tools, and local webhooks
- [docs/board.md](docs/board.md) — Terminal UI layout, keybindings, and status marks

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
