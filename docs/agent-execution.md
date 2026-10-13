# How an agent runs in `gewu-multiverse`

This document describes what happens when you start an agent. It describes the tool
as it is now: an agent runs as an ordinary local child process, with an instruction, a
skill set and a declared list of shell steps, and leaves a log and an append-only
transcript. **No model is called yet**, and no request is made to the Forgejo yet; both
are named where they would appear.

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
      steps.txt        the shell steps one run executes, one per line; `t`
      skills/          one directory per skill, each with a SKILL.md; `K`
      workspace/       where a run's declared steps work
  ```

- The **state root** (`~/.local/state/gewu-multiverse`, or `$XDG_STATE_HOME/gewu-multiverse`)
  holds one directory per agent, and one directory per run:

  ```
  <name>/<run id>/
      plan.json         what the run was started with, written before it starts
      run.log           the agent process's own output and the declared steps' output
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
the declared steps and the per-run timeout. Nothing secret is in it, and nothing secret
is passed on a command line.

### Starting

One child process per agent is started, with the agent's workspace as its working
directory, as the leader of a **new process group**. That group is what makes stopping
work: one signal reaches the agent and every tool it started.

The child then, in sequence:

1. records that the run started, with its own process id;
2. installs the agent's skills into the run directory, byte for byte — a skill is one
   directory with a `SKILL.md`; an unsafe skill name, a skill without a manifest, or a
   link inside a skill refuses the whole run, so nothing is half-installed;
3. reads `instruction.md` and records its size and a digest of its bytes;
4. executes the declared steps, one shell command at a time, in the workspace, through
   `/bin/sh -c`, recording each call and its result in the transcript;
5. records its own exit and the run's outcome.

The **declared tool set** is exactly one tool: `shell`, run inside the workspace. Until
model calls exist, the steps come from the agent's own `steps.txt`. Delete that file and
a run declares no steps. A step that fails ends the run, and later steps do not run.

### Finishing

The outcome is decided by the process, never by what it wrote:

| How it ended | Outcome |
|---|---|
| the process exited 0 and every step succeeded | `succeeded` |
| the process exited non-zero, or a step failed | `failed` |
| the tool stopped it on request | `stopped` |
| the tool stopped it for its timeout | `timed_out` |

The agent process writes the terminal record when it ends on its own. When the tool
stops a run, the process cannot write anything, so the tool writes the terminal record
instead — exactly one terminal record per run, either way.

### The log and the transcript

`run.log` holds the agent process's own lines and the output of every declared step.

`transcript.jsonl` is append-only, one JSON object per line, each with an `event` tag:
`run_started`, `instruction_read`, `skills_discovered`, `tool_call`, `tool_result`,
`child_exit`, `run_finished`. Nothing already written is ever rewritten. A file whose
last line has no newline after it is the remains of a crash: that incomplete line is
dropped before the next record is appended, so a new record is never written onto it.

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

## The workspace is a layout, not a sandbox

This matters enough to say twice.

An agent runs **as you**: the same user, the same machine, your rights. Its workspace is
a directory and its steps are started with that directory as their working directory —
that is the whole intent, and it is **not confinement**. A step can read anything you
can read, including the credentials file, and it can write anywhere you can write. The
tool deliberately asks the operating system for no isolation: no container runtime, no
service, no privilege change. The honest statement is that the workspace is a layout you
can reason about, not a boundary that holds.

## What is not here yet

- **The model call.** A run follows its declared steps; it does not decide them. The
  model that would choose steps is the next round, and the instruction screen says so.
- **The Forgejo work.** No request is made, no role is checked and no repository is
  looked at. Each run's workspace is a plain directory, not a clone from the Forgejo.
- **Secrets.** No run reads the credentials file, because nothing yet needs one.
