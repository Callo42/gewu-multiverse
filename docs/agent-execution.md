# How an agent runs

This document describes what happens when you start an agent. An agent is a Forgejo
account with an instruction; starting it makes the engine run one run after another,
with the same instruction, until the person stops it. Each run calls the model, uses
the declared tools, and leaves a log and an append-only record. The Forgejo side is
described in [The bundled Forgejo](forgejo.md).

## What an agent is

An agent has a **name**: lowercase letters, digits and single hyphens, at most 32
characters, and safe as one directory name. Its Forgejo login is derived from that name
as `gewu-<name>`; nothing else about it is a separate configuration file.

Two roots hold everything. Both are made absolute when the layout is built: a relative
`--config-dir` or `--state-dir` is taken from the directory the program was started in,
so a child started in the agent's workspace still reads and writes the same paths. The
roots, each agent's state directory and each run directory are 0700; the files in them
are 0600.

- The **configuration root** (`~/.config/gewu-multiverse`, or
  `$XDG_CONFIG_HOME/gewu-multiverse`) holds `multiverse.toml` (the product's small
  settings, no secret), `credentials.toml` (0600, the keys), and one directory per
  agent:

  ```
  agents/<name>/
      instruction.md   what the agent is told to do; you write it with `e`
      skills/          one directory per skill, each with a SKILL.md, if you add any
      workspace/       where every tool works
  ```

- The **state root** (`~/.local/state/gewu-multiverse`, or
  `$XDG_STATE_HOME/gewu-multiverse`) holds one directory per agent, and one directory
  per run:

  ```
  <name>/<run id>/
      plan.json         what the run was started with, written before it starts
      forgejo.json      the non-secret repository context, when there is one
      run.log           the agent process's own output and every tool's output
      transcript.jsonl  the append-only record of what happened
      skills/           the skill set this run installed, byte for byte
      session.pid       the running process, removed when it is reaped
  ```

A run id is a timestamp and a sequence number (`run-<milliseconds>-<sequence>`), so
sorting run ids sorts runs by age. Run records are read from disk, so a run started
before the program was reopened is read just like one started now.

## The loop

The person's started/stopped choice is stored in `multiverse.toml`. `Start` records it
and the engine tries a run; `Stop` clears it and ends the run in flight. When a run
ends, for any reason, and the agent is still started, the next one is scheduled:

- a run that succeeded, or ran at least 60 seconds, waits **3 seconds**;
- a quick failure waits **10 seconds**, doubling with each consecutive quick failure,
  capped at **5 minutes** (10, 20, 40, 80, 160, 300, 300 …).

Before a run the engine checks, in order, that there is a model address, name and key,
a non-empty instruction, a running Forgejo whose first setup has returned the
organisation's repositories, and a ready agent account. If one is absent the card says
`needs a model`, `needs an instruction`, `waiting for Forgejo` or `account not ready`;
the check runs on every tick, so the agent starts by itself when the cause is removed.
The repository check is what gives the first run after a reopen or a recovery its
`forgejo.json` and the six tools instead of a shell-only run.

Before every run the engine clones each `team` repository (at most 20) into the
agent's workspace as the agent, or fetches it if the clone is already there. A present
clone is never reset, checked out or discarded; local work stays. The non-secret
context naming the login and the clones is written into the run directory as
`forgejo.json`.

## Preparing a run

The engine creates the run directory, writes `plan.json` into it, and starts one child
process with `--run-agent <run-dir>` and the agent's workspace as its working directory.
The plan holds the agent's name, its workspace, its instruction, its skills directory,
the per-run timeout, and the model facts: the one address, the model id, the path of
the credentials file, and the bound on model turns. **The key is not in the plan.** The
plan holds the *path* of the credentials file; the run reads the key from that file
when it needs it. Nothing secret is passed on a command line.

The child runs as the leader of a **new process group**. That group is what makes
stopping work: one signal reaches the agent and every tool it started.

## What one run does

The child, in sequence:

1. records that the run started, with its own process id;
2. installs the agent's skills into the run directory, byte for byte — a skill is one
   directory with a `SKILL.md`; an unsafe skill name, a skill without a manifest, or a
   link inside a skill refuses the whole run, so nothing is half-installed;
3. reads the instruction document the plan names and records its size and a digest of
   its bytes; the product's own placeholder line is dropped, and a document that is
   nothing but the placeholder is refused;
4. reads its model key out of the credentials file — before anything runs, so that any
   output can be redacted with it — and records the bounds it will be held to;
5. **calls the model**, one turn at a time, until the model answers without a tool call
   or the turn bound is reached;
6. records its own exit and the run's outcome.

The **declared tool set** is `shell`, run inside the workspace, and — only when all
three hold — six Forgejo tools. The three gates are: a Forgejo context was written for
the run; the agent's identity is settled and its own key is in the credentials file;
and at least one repository is cloned into its workspace. Otherwise the run declares
`shell` alone. The model is told each tool's name, what it does, and the JSON schema of
its arguments.

`shell` takes one argument, `command`, and runs it with the workspace as its working
directory. The Forgejo tools take a repository (`owner/name`), an issue or pull-request
number, and a title, body or comment as the act needs. They may only use the agent's
cloned repositories; anything else is refused with one sentence naming the ones it may
use. Their authority is deliberately narrow:

- `forgejo_list_assigned_issues` — read the open issues assigned to the agent's own
  account in its cloned repositories;
- `forgejo_read_issue_or_pull` — read one issue or pull request and its comments,
  bounded (8 KiB of body, 2 KiB of a comment, 20 comments) with the cut declared;
- `forgejo_comment` — write one comment, at most 8 KiB;
- `forgejo_list_my_pull_requests` — read the agent's own open pull requests;
- `forgejo_open_pull_request` — open a request from the clone's checked-out branch;
- `forgejo_update_pull_request` — change the title/body of the agent's own open request
  for that branch.

A comment, an opened request and an update are writes and each counts against a
**per-run bound of eight writes**. After eight, the write tools refuse with one
sentence naming the bound, while reads continue. The tools never merge, close, reopen,
delete, label, assign, approve a review, or change a repository or a user.

The Forgejo tools use the agent's **own** key, never the administrator's. The product
reads it from the credentials file at the call and puts it only in the request's
`Authorization` header. It is not in a model message, a tool result, the transcript,
the log, `shell`'s environment or the workspace. Every Forgejo failure reaches the
model as one bounded sentence naming the class and status, never the raw reply body.

## The model exchange

A run posts one request per turn to the one model address — directly, with no relay —
carrying the model id in the body and the key in the `Authorization` header. The
request shape is the OpenAI-compatible chat-completions shape: a `model`, the
`messages` so far, and the declared `tools`. The exchange is **not streamed**: one
request, one reply, one transcript record, which is what makes a run readable
afterwards.

Each turn:

1. the request is recorded in the transcript as it is sent, with the key redacted;
2. the reply is recorded as it arrived, with its text, its tool calls and its usage;
3. if the reply holds tool calls, each one is run in the workspace and its result is
   recorded and fed back to the model;
4. if the reply holds no tool call, the model has finished and the loop ends.

Everything the model is given and everything it is told back is redacted with the
credentials file's secrets — the model key, the Forgejo administrator key, and the
agents' keys — before it is written down, so a run's records never hold a secret even
if a tool printed the credentials file.

## The bounds

Two declared bounds stop a loop that would not stop itself, and both are in the run's
record:

| Bound | Where it is declared | What happens when it is reached |
|---|---|---|
| `max_turns` | the one model setting (default 24; a value below 1 is treated as 1) | the run ends as `failed`, with a reason naming the maximum and the `max_turns` bound |
| the run timeout | the engine's session timeout (default 900 s) | the engine stops the run, and it is recorded as `timed_out` |

The turn bound is written into the transcript (`run_limits`) before the first call, so
a run read afterwards shows what it was held to rather than only how it ended. The run
timeout also bounds each individual request: a request never waits longer than the time
the run has left.

## Finishing

The outcome is decided by the process, never by what it wrote:

| How it ended | Outcome |
|---|---|
| the process exited 0: the model finished | `succeeded` |
| the process exited non-zero, or a model call failed | `failed` |
| the person stopped it | `stopped` |
| the engine stopped it for its timeout | `timed_out` |

The agent process writes the terminal record when it ends on its own. When the engine
stops a run, the process cannot write anything, so the engine writes the terminal
record instead — exactly one terminal record per run, either way.

## The log and the transcript

`run.log` holds the agent process's own lines and the output of every tool.

`transcript.jsonl` is append-only, one JSON object per line, each with an `event` tag:
`run_started`, `instruction_read`, `skills_discovered`, `run_limits`, `tool_call`,
`tool_result`, `model_request`, `model_reply`, `model_call_failed`, `forgejo_call`,
`forgejo_result`, `child_exit`, `run_finished`. A model-asked shell call uses
`tool_call`/`tool_result`; a Forgejo call uses `forgejo_call`/`forgejo_result`, naming
the tool, repository, number and outcome, and a successful write carries its action
word and the web URL of what it changed. Nothing already written is ever rewritten. A file whose last line has
no newline after it is the remains of a crash: that incomplete line is dropped before
the next record is appended, so a new record is never written onto it.

## Stopping, and opening again

Stopping a run signals its whole process group — so tools the agent started are stopped
too — politely first (`SIGTERM`), and forcibly (`SIGKILL`) after a short grace. The
engine then reaps the child and sweeps the group again, so a run leaves nothing of
itself running. Leaving the board stops every run the same way, within a budget.

Opening the program again starts the Forgejo and the agents that were left started,
with the same instructions. A started agent waits until Forgejo is running and its
repositories have come back, so its first run has the Forgejo context and the six
tools. If the program was killed outright, the next open finds
the recorded Forgejo and run pids, believes one only when `/proc/<pid>/cmdline` still
names the command this product started, stops it, and only then opens.

## Editing an instruction, and reading a run

The board does not grow its own editor. It hands a path to the program you already
chose: `$EDITOR` (or `$VISUAL`, or `vi`) for editing, `$PAGER` for reading a log or
transcript. The path is always passed as an argument, never pasted into command text.
The terminal is put back exactly as it was when the program exits.

On an agent's screen, `e` edits `agents/<name>/instruction.md` (the editor creates it
if it is absent), `l` opens the newest run's `run.log`, and `t` opens
`transcript.jsonl`. A hand-off steps out of the terminal and restores it exactly.

## The workspace is a layout, not a sandbox

This matters enough to say twice.

An agent runs **as you**: the same user, the same machine, your rights. Its workspace
is a directory and its tools are started with that directory as their working
directory — that is the whole intent, and it is **not confinement**. A tool can read
anything you can read, including the credentials file, and it can write anywhere you
can write. The product deliberately asks the operating system for no isolation: no
container runtime, no service, no privilege change. The honest statement is that the
workspace is a layout you can reason about, not a boundary that holds.

Because an agent can read the credentials file, the run does not rely on the boundary
for secrecy: the key and every other secret in that file are redacted from every string
the run writes down — a tool's output, a model reply, an error message — before it
reaches the transcript or the log. The Forgejo tools do not make the credentials file
unreachable to a shell command: `shell` still runs as you, with your rights, and can
read the file as before.

## What is not here

- **Streaming a reply.** The model exchange is one request and one reply. A streamed
  exchange is not here.
- **A scheduler.** A run starts when the agent is started, and the loop keeps it going
  until it is stopped. There is no timer and no cron.
- **A per-agent model or a second Forgejo.** One address, model and key serve every
  agent; one bundled Forgejo serves the installation.
