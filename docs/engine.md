# The engine behind the board

The board and the engine are two halves of one program. The board draws; the engine
owns the bundled Forgejo, the agents' accounts, and the runs that keep going until they
are stopped. The contract between them is `src/engine_api.rs`: one `BoardView` per
frame, commands that return at once, and slow work on background threads.

`src/main.rs` opens the engine with `engine::open_with(EngineOptions)`. `--demo` opens
the same board on the scripted engine in `board::fake`; `--install-forgejo` runs the
installer and exits; `--run-agent` is one agent run without a board.

## One Forgejo, installed once

The pinned release is Forgejo 17.0.0 for Linux amd64, SHA-256
`5a3b07e124ed70b4e46ee3939515f003bd3cc85352257a9a4607a527251042ca`. The binary is
installed under the data root at `gewu-multiverse/forgejo/`: streamed to a temporary
file beside the target, hashed, and renamed into place only when the digest matches. A
mismatch removes the temporary file and keeps nothing.

The sources are tried in order:

1. this release's own GitHub asset,
   `https://github.com/Callo42/gewu-multiverse/releases/download/v<crate version>/forgejo-17.0.0-linux-amd64`;
2. the official Codeberg release,
   `https://codeberg.org/forgejo/forgejo/releases/download/v17.0.0/forgejo-17.0.0-linux-amd64`.

Every answer is checked against the same pinned digest; a missing or wrong first
source falls through to the second with no question. Only when the whole list fails
does the install refuse, with one line saying the next start tries again. Nothing is
kept from a failed source. `GEWU_FORGEJO_BINARY` names a local file, whose SHA-256 is
checked the same way, so a test never needs the network.

The first start installs the binary by itself when it is missing: the board's build
picture reports `downloading n%` while it streams. `--install-forgejo` runs the same
installer on its own, and `install.sh` calls it once so the first run has nothing to
fetch.

An instance has its own directory beside the binary:

```
forgejo/
  forgejo-17.0.0-linux-amd64   0755, hash-checked
  engine.lock                 0600, flock: one program per data directory
  instance/
    app.ini                   0600; generated secrets, this-computer-only address, SQLite, no SSH/mail/actions
    secrets/admin-password    0600; generated once, never printed by the engine
    secrets/admin-login       0600; the login the instance was made with
    log/forgejo.log
    data/forgejo.db, data/repositories/
```

The data root comes from the platform's data directory: on Linux
`~/.local/share/gewu-multiverse` (or `$XDG_DATA_HOME/gewu-multiverse`).

The port is chosen once from 3000–3099 and then read back from `app.ini`, so a restart
uses the same address. `migrate` runs before the first administrative change; the
person's administrator account (login from `$USER` when it is a valid OS login, else
`owner`) is made through Forgejo's own `admin user` CLI. The process runs as `web` in
its own process group. It is ready when `/api/v1/version` answers.

Provisioning through the administration API is idempotent: organisation `team`, team
`agents` with write access to all its repositories (its units are code, issues, pull
requests and projects, all write, so Forgejo's own summary says `write`), and the
starter repository `team/workspace` initialised with a README.

The lock is held for as long as the engine is open. A second program that points at the
same data directory fails at `open` with one sentence; it never touches the first
program's instance.

### Forgejo states

| State | Meaning | What moves it on |
|---|---|---|
| `Missing` | no checked binary yet | `StartForgejo` starts the install |
| `Fetching { percent }` | installing, percent when the size is known | install result |
| `Preparing` | writing `app.ini`, migrating, making the admin account | prepare result |
| `Starting` | the `web` child is running, waiting for the version route | the route answering |
| `Running` | answering on the this-computer address | the child dying, or `shutdown` |
| `Stopped` | never started, or the program is closing | `StartForgejo` |
| `Failed { reason }` | install, prepare or start failed, or the child died | a scheduled retry, with back-off |

A failed fetch is not retried behind the person's back; the one line says the next
start tries again. A child that dies while the engine is open leaves `Failed` with one
plain sentence, and a restart is scheduled: 3 s, 6 s, 12 s … capped at 60 s, reset when
it has been running for a while. `shutdown(grace)` sends `SIGTERM` to the child's group,
waits up to the grace, then sends `SIGKILL`, reaps it, and confirms no process is left.

### Left-over processes and the program's own ending

The running server writes its pid into `forgejo/instance/forgejo.pid`; each session
writes `session.pid` in its run directory. The files are removed when the processes
are reaped. If the program is killed outright, the next `open` reads those records,
believes a pid only when `/proc/<pid>/cmdline` still names the expected command (this
instance's `app.ini` and `web` for Forgejo; the run directory and `--run-agent` for a
session), and stops the process group politely then by force. It never guesses from a
bare number, and a reused pid is never killed. Only after that recovery does the engine
start a fresh Forgejo, so two servers never share one data directory.

`q`, the terminal closing (`SIGHUP`) and the window manager ending the program
(`SIGTERM`) all take the same path: stop every session and Forgejo, leave the alternate
screen, and print one line when an agent was started (it resumes when the program is
opened again).

## Older files are left alone

The engine's own settings are `multiverse.toml` in the configuration root:
`~/.config/gewu-multiverse/multiverse.toml` (or `$XDG_CONFIG_HOME/gewu-multiverse`).
It records whether the terms were acknowledged, the one model address and name, the
person's administrator login, and each agent's name and started/stopped choice. It
holds no secret.

Both roots are made absolute when the layout is built. A relative `--config-dir` or
`--state-dir` is taken from the directory the program was started in, so the child
started in the agent's workspace reads the plan, the credentials and the Forgejo
context from the same absolute paths the engine wrote. The configuration root, the
state root, each agent's state directory and each run directory are 0700; the files
written into them are 0600.

An installation made by the old text screens may still hold `config.toml`. The engine
never reads, converts or writes it. At open it checks whether the file exists and, if
it does, puts one line on the first screen: `an earlier setup was found and left as it
was`. The old file is otherwise ignored, and the new settings start as if the
installation were fresh.

The secrets are in `credentials.toml` beside it, created with mode 0600: the bundled
Forgejo's administrator token, the one model key (top-level `model_key`), and each
agent's own Forgejo token under `[forgejo_agent_tokens]`. A value is read only when it
is needed and is never printed.

## Agents and one model

`CreateAgents { count }` makes `agent-1` … `agent-N`; `AddAgent` makes the next free
`agent-N`. Each name derives its Forgejo login `gewu-<name>`: the account is created
and marked, the token is made with repository, user and issue rights, and the account
joins team `agents`. Membership is idempotent. Provisioning runs on a background
thread and retries while Forgejo is up; the card shows `Pending`, `Ready` or
`Failed { reason }`.

`RemoveAgent` stops the agent, deletes its token on the server by the name recorded
when the token was made, removes it from team `agents`, deletes the dead token value
from the credentials file, and forgets the agent in the settings. The account, its
commits and its run records stay; removal is not a history rewrite.

The model is one set of three facts — address, model, key — used by every agent. The
key is written to the shared top-level `model_key` in the credentials file.
`TestEndpoint` runs the model probe in the background and leaves its result in the
view.

## One agent's run loop

The started/stopped choice is written into `multiverse.toml`. Reopening the program
reads that choice back; started agents resume.

A **run** is one existing agent run: a fresh run directory, the same instruction, and
the non-secret Forgejo context naming the agent's login and its cloned `team`
repositories, so the run declares `shell` and the six Forgejo tools. Before each run
every repository of `team` (at most 20) is cloned into the agent's workspace as the
agent, or fetched if it is already there. A clone that is present is never reset,
checked out or discarded; local work stays.

The state the card draws, and the edges between them:

| State | Meaning | Edges |
|---|---|---|
| `Idle` | stopped: no run is in flight and none will start | `Start` → tries a run |
| `Running { number, since }` | one run is in flight | the run ending, `Stop`, timeout |
| `Restarting { next_at, last }` | started, waiting for the back-off | `next_at` → new run; `Stop` → `Idle` |
| `Blocked { reason }` | started but a cause prevents a run | cause gone → new run; `Stop` → `Idle` |

The causes checked before a run, in order, each with one plain reason: `needs a model`
(no address, model or key), `needs an instruction`, `waiting for Forgejo`, and `account
not ready`. `waiting for Forgejo` covers the server not running, its provisioning job
not finished, and the organisation's repository list not returned yet: the first run
after a reopen or a recovery must carry `forgejo.json` and the six tools, so it is not
allowed to start in the gap between `Running` and provisioned. `Blocked` is re-evaluated
on every tick, so the agent starts by itself as soon as the cause is removed.

When a run ends, for **any** reason — success, a non-zero exit, a timeout, a crash —
and the agent is still started, the next one is scheduled:

- succeeded, or ran at least 60 s: **3 s**, and the quick-failure streak resets;
- otherwise: **10 s**, doubling with each consecutive quick failure, capped at
  **5 min** (10, 20, 40, 80, 160, 300, 300 …).

`Stop` sends the existing polite `SIGTERM` to the run's process group; after the grace
the engine sends `SIGKILL`. It clears any scheduled run, so nothing new starts. A run
that overruns the session timeout is stopped the same way and recorded as `TimedOut`.

`recent` is the newest ten endings, reconstructed from the run records on open and
updated as runs end. `activity` is the newest Forgejo action of the newest run, read from
that run's transcript.

## Threads, ticks and the contract

`board()` reads memory only: no network, no process start. `send()` records the command
and returns; the work it implies runs on a background thread. `tick()` (about four times
a second) reaps ended runs, starts due ones, follows the Forgejo child, and refreshes
the instruction summaries. `shutdown(grace)` stops every run and the Forgejo within the
grace, joins the children, and leaves no process behind. The engine keeps the data lock
until it is dropped.

The clocks are an interface, not a call to `SystemTime::now()` scattered through the
loop, so the tests drive a fake clock and a fake run runner: restart after success and
after failure, the exact back-off sequence, its cap and its reset, `Stop` during a run
and during a back-off, `Blocked` and its release, and resume after reopening.

## The words on screen

The terms the board shows are the five fixed sentences in `engine::TERMS`; the
engine's reasons, notices, activities and model-test lines are written in the same
plain words the board uses, so no technical word reaches a screen from either side.
`src/board/status.rs` is the one cleaner for free text and also removes the backticks
the engine writes around names.
