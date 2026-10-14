# How an agent runs in `gewu-multiverse`

This document describes what happens when you start an agent. It describes the tool
as it is now: an agent runs as an ordinary local child process, with an instruction, a
skill set and a declared list of shell steps, calls the model at its own configured
endpoint, runs the tool calls the model asks for inside its workspace, and leaves a log
and an append-only transcript. The Forgejo side is described in
[Connecting a Forgejo](forgejo.md): the role check, one identity per agent, and the
clone an agent works in.

## What an agent is

An agent has a **name**. Everything else about it is derived from that name, so adding
an agent is a configuration change and never a code change. A name is lowercase
letters, digits and single hyphens, at most 32 characters, and it must be a name that
can safely become one directory. A name that cannot is refused with a sentence, before
any path is built from it.

Two roots hold everything.

- The **configuration root** (`~/.config/gewu-multiverse`, or `$XDG_CONFIG_HOME/gewu-multiverse`)
  holds `config.toml`, `credentials.toml`, the first-run acknowledgment, and one
  directory per agent:

  ```
  agents/<name>/
      instruction.md   what the agent is told to do; you write it with `e`
      steps.txt        shell steps one run executes first, as a fixed prelude; `t`
      skills/          one directory per skill, each with a SKILL.md; `K`
      workspace/       where every tool works
  ```

- The **state root** (`~/.local/state/gewu-multiverse`, or `$XDG_STATE_HOME/gewu-multiverse`)
  holds one directory per agent, and one directory per run:

  ```
  <name>/<run id>/
      plan.json         what the run was started with, written before it starts
      run.log           the agent process's own output and every tool's output
      transcript.jsonl  the append-only record of what happened
      skills/           the skill set this run installed, byte for byte
  ```

A run id is a timestamp and a sequence number (`run-<milliseconds>-<sequence>`), so
sorting run ids sorts runs by age. Run records are read from disk, so a run started in
an earlier session is listed just like one started now.

## What a run does

A run is one execution of one agent. The tool prepares it, starts one child process,
and decides how it ended.

### Preparing

The tool creates the run directory, writes `plan.json` into it, and creates `run.log`.
The plan holds the agent's name, its workspace, its instruction, its skills directory,
the declared steps, the per-run timeout, and the model configuration: the endpoint, the
model id, the path of the credentials file, and the bound on model turns. **The key is
not in the plan.** The plan holds the *path* of the credentials file; the run reads the
key out of that file when it needs it. Nothing secret is passed on a command line.

### Starting

One child process per agent is started, with the agent's workspace as its working
directory, as the leader of a **new process group**. That group is what makes stopping
work: one signal reaches the agent and every tool it started.

The child then, in sequence:

1. records that the run started, with its own process id;
2. installs the agent's skills into the run directory, byte for byte — a skill is one
   directory with a `SKILL.md`; an unsafe skill name, a skill without a manifest, or a
   link inside a skill refuses the whole run, so nothing is half-installed;
3. reads the instruction document the configuration names (the plan carries its
   resolved path) and records its size and a digest of its bytes;
4. reads its model key out of the credentials file — before anything runs, so that any
   output can be redacted with it — and records the bounds it will be held to;
5. runs the declared steps, one shell command at a time, in the workspace, through
   `/bin/sh -c`, recording each call and its result in the transcript; a step that
   fails ends the run there, and later steps do not run;
6. **calls the model**, one turn at a time, until the model answers without a tool call
   or the turn bound is reached (below);
7. records its own exit and the run's outcome.

The **declared tool set** is exactly one tool: `shell`, run inside the workspace. The
model is told its name, what it does, and the JSON schema of its one argument
(`command`). Each call runs the command with the workspace as its working directory,
and its output is returned to the model.

### The model exchange

A run posts one request per turn to the endpoint its agent's configuration names —
directly, with no relay — carrying the model id in the body and the key in the
`Authorization` header. The request shape is the OpenAI-compatible chat-completions
shape: a `model`, the `messages` so far, and the declared `tools`. The exchange is
**not streamed**: one request, one reply, one transcript record, which is what makes a
run readable afterwards.

Each turn:

1. the request is recorded in the transcript as it is sent, with the key redacted;
2. the reply is recorded as it arrived, with its text, its tool calls and its token
   usage;
3. if the reply holds tool calls, each one is run in the workspace and its result is
   recorded and fed back to the model;
4. if the reply holds no tool call, the model has finished and the loop ends.

Everything the model is given and everything it is told back is redacted with the
credentials file's secrets — the run's own key, the Forgejo tokens, and the other agents'
keys — before it is written down, so a run's records never hold a secret even if a
tool printed the credentials file.

### The bounds

Two declared bounds stop a loop that would not stop itself, and both are in the run's
record:

| Bound | Where it is declared | What happens when it is reached |
|---|---|---|
| `max_turns` | the agent's configuration (1 to 256; default 24) | the run ends as `failed`, with a reason naming the maximum and the `max_turns` bound |
| `run_timeout_secs` | `[limits]` in the configuration | the tool signals the run, and it is recorded as `timed_out` |

The turn bound is written into the transcript (`run_limits`) before the first call, so
a run read afterwards shows what it was held to rather than only how it ended. The run
timeout also bounds each individual request: a request never waits longer than the time
the run has left.

### Finishing

The outcome is decided by the process, never by what it wrote:

| How it ended | Outcome |
|---|---|
| the process exited 0: the model finished, and every declared step succeeded | `succeeded` |
| the process exited non-zero, a declared step failed, or a model call failed | `failed` |
| the tool stopped it on request | `stopped` |
| the tool stopped it for its timeout | `timed_out` |

The agent process writes the terminal record when it ends on its own. When the tool
stops a run, the process cannot write anything, so the tool writes the terminal record
instead — exactly one terminal record per run, either way.

### The log and the transcript

`run.log` holds the agent process's own lines and the output of every tool.

`transcript.jsonl` is append-only, one JSON object per line, each with an `event` tag:
`run_started`, `instruction_read`, `skills_discovered`, `run_limits`, `tool_call`,
`tool_result`, `model_request`, `model_reply`, `model_call_failed`, `child_exit`,
`run_finished`. A declared step and a model-asked tool call use the same `tool_call` and
`tool_result` records; the model-asked ones carry the turn that asked for them, and a
declared step's turn is `null` (and is written as absent). Nothing already written is
ever rewritten. A file whose last line has no newline after it is the remains of a
crash: that incomplete line is dropped before the next record is appended, so a new
record is never written onto it.

## Timeouts and how many agents run at once

Both are configuration:

```toml
[limits]
run_timeout_secs = 900        # 1 to 86400; default 900
max_concurrent_agents = 10    # 1 to 64; default 10
```

A run that reaches its timeout is signalled and recorded as `timed_out`, with the
reason on screen and in the transcript.

At most `max_concurrent_agents` agents run at once. A start beyond that limit **waits
its turn**; it does not fail. The agent list shows how many are running and how many are
waiting, and the waiting start becomes a running one as soon as a slot is free. Calling
off a start that is still waiting leaves a `stopped` run behind.

## Stopping

Stopping a run signals its whole process group — so tools the agent started are stopped
too — politely first (`SIGTERM`), and forcibly (`SIGKILL`) after a short grace. The tool
then reaps the child and sweeps the group again, so a run leaves nothing of itself
running. Leaving the UI stops every run the same way, within a budget.

An agent in the opt-in `systemd` mode is stopped by asking the user's own
`systemctl --user` to stop its unit instead, and the run is recorded as `stopped` just
the same. Such an agent is deliberately **not** stopped when the UI exits: that is the
reason the mode exists. See [Two ways an agent can be supervised](systemd-mode.md).

## Editing an instruction, skills and steps

The UI does not grow its own editor. It hands a path to the program you already chose:
`$EDITOR` (or `$VISUAL`, or `vi`) for editing, `$PAGER` for reading a log. The path is
always passed as an argument, never pasted into command text. The terminal is put back
exactly as it was when the program exits.

Keys on the instruction screen:

- `e` — edit `instruction.md` (created from a short template if it is absent);
- `K` — hand the `skills/` directory to your editor (create one directory per skill,
  each with a `SKILL.md`);
- `t` — edit `steps.txt`.

## Testing an endpoint

Pressing `p` on an agent's screen makes **one minimal call** to that agent's endpoint:
one short user message, no tools, a handful of reply tokens. What comes back is shown
on the agent screen and in the status line, either as the model's reply or as the class
of failure — `bad_url`, `missing_key`, `bad_key`, `model_not_found`, `rate_limited`,
`server_error`, `malformed_reply`, `timeout` or `transport`. The key is read from the
credentials file for that one call and is never shown.

## The workspace is a layout, not a sandbox

This matters enough to say twice.

An agent runs **as you**: the same user, the same machine, your rights. Its workspace is
a directory and its tools are started with that directory as their working directory —
that is the whole intent, and it is **not confinement**. A tool can read anything you
can read, including the credentials file, and it can write anywhere you can write. The
tool deliberately asks the operating system for no isolation: no container runtime, no
service, no privilege change. The honest statement is that the workspace is a layout you
can reason about, not a boundary that holds.

Because an agent can read the credentials file, the run does not rely on the boundary
for secrecy: the key is redacted from every string the run writes down — a tool's
output, a model reply, an error message — before it reaches the transcript or the log.

## What is not here yet

- **Streaming a reply.** The model exchange is one request and one reply. A streamed
  exchange would fill the same reply before the turn ends; it is not here yet.
- **The clone inside the workspace is not made for you on start.** You clone a
  repository from the Forgejo screen (`g`); a run does not clone one by itself. See
  [Connecting a Forgejo](forgejo.md).
