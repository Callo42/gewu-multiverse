# Agent execution in gewu-multiverse

This document describes how an agent runs in `gewu-multiverse`: what the tool is, what each part of
an agent is, and how a run behaves. It is written for this repository's own documentation and uses
only this repository's vocabulary.

## What the tool is

`gewu-multiverse` is a local, single-Universe Matrixverse written in Rust. One Forgejo hosts the Git
repositories. A configurable number of agents work in it. The TUI is the only entrypoint: there is no
web page, no service API for users, and no account or tenant model.

Three choices shape everything below:

- **One child process per agent.** An agent is a long-lived process that starts one agent run at a
  time, in its own workspace, as an ordinary child process.
- **Direct endpoints, one credentials file.** Agents talk to the Git server and to the model endpoint
  directly. There is no broker between an agent and an endpoint. A single credentials file holds the secrets that runs need, restricted by file permissions to your user. Note plainly: an agent
  runs **as you**, so it can read anything you can — including that file. See *Running agents is not a sandbox* in the README.
- **One Forgejo.** Repositories, permissions and merge requests are the Forgejo's own; the tool does
  not keep a second list of who may do what.

## What an agent is

An agent is made of six parts. Each is editable or replaceable on its own.

### Identity

An agent has a name. Everything else about it — its workspace directory, its record directory, its
configuration file, the names of its local sockets and log files — is derived from that name, so
adding an agent is a configuration change and never a code change. Names are bounded and validated:
lowercase letters, digits and hyphens, no leading or trailing hyphen, no doubled hyphen, within a
length that keeps every derived path and socket name inside the filesystem's own limits. Two agents
can never derive the same workspace: a derived path that would sit inside another agent's workspace
is refused.

### Configuration

One roster file lists the agents that exist and whether each is set to run or paused. One file per
agent carries that agent's own settings: its workspace repository, its instruction, its skill set,
its model, its branch, and the run limits it is admitted with. Both files are validated strictly
when read: unknown fields are refused, a roster count that disagrees with its list is refused, a
duplicate name is refused, and an agent whose settings disagree with the roster is refused. Editing
these files is the way to add, remove, pause or reconfigure an agent.

### Workspace

An agent's workspace is one clone of one Git repository. A run begins with a fresh clone, so a run
never inherits a previous run's uncommitted state. The clone is made from the local Forgejo, and the
agent's commits carry the agent's own name.

### Instruction

An instruction is a plain-text document on disk. It has two layers:

- a **fixed header** written by the tool, carrying the identity facts the agent must be told (its
  name, the Universe it works in, the repository it owns) with the tool substituting those facts;
- an **editable body**, which the user writes, stored as a versioned document with a content digest
  and a timestamp.

The body may be set for all agents or overridden for one agent. Every write keeps the previous
versions, so history is never destroyed. A body takes effect at the **next** run: a run that has
already started keeps the instruction it started with. Bodies are bounded plain text — no binary
content, no credential-shaped text — and a body that names a skill which is not installed is refused
rather than delivered.

### Skills

A skill is a directory containing a `SKILL.md` manifest and optionally more plain-text files. A
skill set is either the tool's built-in set or a set the user configures. At the start of a run the
tool writes the effective set into the agent's home directory, refusing unsafe names, empty skills
and paths that would leave the skill's own directory. Skill bodies are never pasted into the
instruction: the agent discovers and loads them itself.

### Endpoints and credentials

Two endpoints are configured: the Forgejo and the model. A run's model request must name the model
that the agent is configured with; a request naming a different model is refused. Request targets are
built from the configured endpoint, never from anything the agent supplies, and a redirect is never
followed while a credential is attached. The single credentials file is read once per run, checked to
be non-empty, in-bounds and free of whitespace and control characters, and never appears in the
instruction, the agent's environment, a log line or a transcript.

## How a run behaves

### Preparing

A run begins by taking a fresh run identity and a fresh per-run secret. The tool then writes the
agent's generated configuration, installs the effective skills, checks that the credential really
identifies the agent it is supposed to (by asking the Git server who the credential is, and refusing
the run if the answer is not the expected agent), and clones the workspace. Each of these steps can
refuse the run; none of them silently continues.

### Executing

The run then starts **one** agent process with the instruction on its standard input, a fixed
argument list, a fixed environment, and the workspace as its working directory. The process runs
until it exits, until the run is stopped, or until the run's deadline is reached.

### Finishing

The outcome of a run is decided by the process, never by what the process wrote. A clean exit
succeeds. A stop requested by the tool is recorded as stopped. A run that reaches its deadline is
recorded as failed for that reason. Stopping a run signals the whole process group — so tools the
agent started are stopped too — first politely, then forcibly after a short grace.

### Lifetime

Each run has one absolute deadline, fixed when the run starts. The deadline reserves a shutdown
grace before it, so the process is stopped early enough to end cleanly. Nothing extends a deadline:
not a retry, not a new turn, not a pause and resume. The next attempt is a new run with a new
deadline.

### Records

Each run writes two things: a JSONL transcript of the run (its identity, timestamps, model calls and
tool effects, with explicit truncation flags where a payload was too large) and a log file carrying
the agent process's own output. Append is crash-safe: a partial final line is recognised as partial
and the next line is never written onto it.

### Supervision

One long-lived supervising process owns each agent. It holds an exclusive reservation for that agent,
so two supervisors can never own one agent at once. After an unclean end it waits a full run-length
window before resuming, so it can never overlap a predecessor that is still finishing. Then it runs a
session loop: read the pause setting, clean up any residue, prepare a run, execute it, classify the
outcome, and either take the success cooldown or the failure backoff.

The supervisor publishes a state for each agent — starting, running, waiting, cooling down, backing
off, paused, stopped — together with the current run, the number of consecutive failures, the reason
for the last failure, and whether cleanup is pending.

### Pausing and waking

Pausing is a setting, not a signal. It is read at the start of a run and never interrupts one in
progress: the running run finishes, its record is written, and the agent then stops cleanly. A pause
setting that cannot be read is not permission to keep going; the supervisor waits and tries again.

A wake request asks the supervisor to start the next run as soon as the current one is done. It
coalesces: many wake requests while a run is active produce one next run, and never interrupt the
active run. This is also what the optional systemd unit's reload action does.

### Concurrency

At most ten agents run at once by default. Each agent may also have a small bound on how many model
requests it keeps in flight, so one agent cannot use the whole model endpoint by itself.

### Failure

Failures fall into two classes, and the difference matters.

- **Retryable**: network and capacity problems. The supervisor waits a bounded, jittered backoff and
  tries again. Consecutive failures lengthen the wait up to a configured maximum. A success resets
  the count and takes a short cooldown instead.
- **Not retryable**: an identity that does not check out, a configuration that fails validation, a
  record whose digest disagrees with its content, a transaction whose result cannot be proved. The
  run stops and the reason is surfaced. Retrying these would only repeat the same wrong thing.

### Stopping and cleanup

When an agent is stopped — by request, by a pause, or by the tool shutting down — cleanup gets its
own time budget, independent of the stop request, so a shutdown can never erase the obligation to
clean up. The tool proves the child process is gone before it declares the agent stopped: an
observed-but-uncertain state is never treated as "already gone". A cleanup failure is recorded as a
cleanup failure, distinct from whatever ended the run in the first place.

## Where runs are controlled

The TUI is the only entrypoint. It lists agents and their states, starts and stops the tool, pauses
and resumes agents, wakes an agent, edits an agent's configuration and its instruction, and manages
its skill set. The same controls are available as documented commands, so the tool can be run without
the TUI where that is useful.

An optional systemd mode runs the tool as services: one supervising process per agent, restarted on
failure, with the same pause, wake and cleanup behaviour. The mode changes only who starts the
supervisor; the run behaviour above is identical either way.
