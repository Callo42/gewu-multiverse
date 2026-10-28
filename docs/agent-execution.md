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

- a run that succeeded, or ran at least 60 seconds, waits **5 seconds**;
- a successful run that found no work, or a warned run, waits the **20 second** watch
  interval (a warning is not a verified success);
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

The list is assigned-only; a read or a comment in a cloned team repository is not
gated by the assignment. A comment, an opened request and an update are writes and each
counts against a **per-run bound of eight writes**. After eight, the write tools refuse with one
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

The process decides whether the run finished; the run's own tools and final summary
decide what a clean finish means:

| How it ended | Outcome |
|---|---|
| exited 0, and a real remote write was recorded (a verified push, an opened or updated pull request, or a comment) | `succeeded`, naming that work |
| exited 0, and a change was left only in the workspace (content, mode, type, link, directory, commit, or a local tag, index, config or ref) | `warned`, naming the local work |
| exited 0, and no tool call was recorded | `warned`, the requested work is unconfirmed |
| exited 0, and the run only read its issue and wrote nothing | `no_work`, with its own sentence |
| exited 0, and a write tool refused a write with no other work, or the summary says it could not do the instruction | `refused` |
| exited 0, and a tool call failed (non-zero exit or a signal) without being a refused write | `warned` |
| exited 0, and the summary says there was nothing assigned to do | `no_work` |
| exited 0, and neither: the model finished | `succeeded` |
| the process exited non-zero, or a model call failed | `failed` |
| the person stopped it | `stopped` |
| the engine stopped it for its timeout | `timed_out` |

The final summary is kept as the terminal record's reason for every clean finish, so
the card can draw it beside `✓` as well as `⊘`/`∅`/`⚠`. A refused Forgejo write, or a
failed shell write (`DELETE`/`PUT`/`POST`/`PATCH` in any standard curl spelling, a
git write read by subcommand position — `push`, `rm`, `mv`, `add`, `commit`, `stash`,
`pull`, `fetch`, `branch`/`tag` changes — `rm`, `rmdir`, `unlink`, `mv`, `cp`, `tee`,
`truncate`, `sed -i`, or a redirection into a file), is evidence from the run rather
than from its prose (review B2). The rule as built (correction 7): the command runs
under `bash` with a `DEBUG` trap installed under `set -T`; the shell reports every
simple command it completes to the product on a private file, and the tool result
carries `segments: [{command, status}]`. The mark reads every raw status the shell
reported — each record's `$?` and every `PIPESTATUS` element — so a run with any
non-zero status is never `✓`: it is `warned` (`⚠`) with the failed command's cause,
or `refused` (`⊘`) when the failed command is a recognised write. Pairing only names
the command; a raw failure the pairing cannot place is `⚠`, never `✓`. A write inside
a compound whose status the shell hides (`if rm …; fi | cat`) is `⊘` when the
operating system's own diagnostic is in the output, and any program's POSIX
diagnostic (`<program-or-shell>: [line N: ][<path>: ]<strerror>`, ending in any error
message the C library itself prints — `Permission denied`, `Read-only file system`,
`File name too long`, `Too many levels of symbolic links` and the rest) is a
witness; an unlisted program's failure there is `⚠`. The shell's own voice is the
third witness: every shell that reads the prelude reports its `$0` (the outer shell
and every nested `bash` that reads `BASH_ENV`), the trap reports `BASH_SOURCE[0]`
(the name bash uses for a sourced command or a function defined there), and bash
prefixes its own errors and warnings with that exact
`<name>: ` — `<name>: line N: <message>` and `<name>: <message>`; a function defined
on the `-c` line is named `environment`. Any line in that form is shell-reported
trouble whatever its text (`ambiguous redirect`, `cannot overwrite existing file` and
every other wording); with an all-zero raw record it makes the run `⚠`, or `⊘` when
the command names a recognised write, or when the shell's record names a write whose
plain target the diagnostic carries. A nested interpreter that does not load the
product's recorder — any non-bash interpreter, or `bash` in POSIX or privileged mode
(`--posix`, `-p`, `POSIXLY_CORRECT=1`) — is limit (e) in `docs/board.md`: its failed
write is seen only through the outer command's exit status and the output
heuristics. A failure with neither a status nor a
diagnostic the system or the shell prints is limit (f) or (g) in `docs/board.md`. A
write that exits non-zero
is a refusal whatever its output says; a failed Forgejo read is a failed call and
also marks the run `warned`.
On a host without `bash`, or when the private record cannot be opened, the product
falls back to reading the output: a failed write behind `; echo done`, `|| true` or
`| tail -5` is still a refusal when its output carries a failure-context line, and a
`curl` without `-f` (which exits 0 on a 4xx/5xx) is read for a status line or an HTTP
error phrase; a write that succeeded is not re-read for words (review corrections 3
and 4). Any other failed tool call marks the run `warned` (`⚠`), never `✓`. A destructive instruction answered only in prose (`I left it in place`,
`I did not delete it`, `deleting it is not safe`) is also a refusal, because the run's
own sentence says the work was not done; a keep phrase beside a success verb for the
destructive act (`I deleted the stale branch and kept the docs as is`) stays a
success.

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

## What an agent run can see

An agent runs **as you**: the same user, the same machine, your rights. Its tools are
started with the workspace as their working directory. The workspace is a layout you
can reason about; it is not, by itself, a boundary. On this computer the product also
asks the operating system for one, and the mechanisms it knows differ:

- **Landlock** is the automatic mechanism. It applies a kernel ruleset to the shell.
  It may read its workspace, its private `TMPDIR` (where its per-run askpass file
  lives), the system's tools (`/usr`, `/bin`, `/sbin`, `/lib`, `/opt`), `/etc`,
  `/proc`, `/sys`, `/run`, `/dev` and the product binary's directory; the
  configuration root, the state root, the Forgejo data root and your real home are
  denied. The names still exist and an attempt answers `Permission denied`. The
  product claims Landlock only after a child process built the ruleset at
  `HardRequirement`, required `FullyEnforced`, and proved it: the child reads an
  allowed control file and is denied a file outside the grant. A kernel where Landlock
  is missing or disabled therefore gives **none**, not a dummy ruleset. Landlock
  grants a whole tree and cannot subtract a rule, so it is **not** chosen when one of
  the hidden roots lies inside a tree it would grant (for example a `--config-dir`
  under `/run`, `/etc`, `/opt`, `/usr`, `/dev`): the method is **none**. A hidden
  root that resolves *inside* the workspace is beyond every mechanism here, because
  Landlock grants the workspace and the namespaces bind it back; that layout is
  **none**. Both sides are resolved with `realpath` first, and the engine enumerates
  every real `<agents>/<agent>/workspace` when it opens and when an agent is added,
  so a state root or workspace that is a link into the other cannot slip past it.
- **bwrap** and **unshare** build a user and mount namespace. They are **opt-in**:
  the product selects one only when you start it with `GEWU_CONFINEMENT=bwrap` or
  `GEWU_CONFINEMENT=unshare`. Their complete argument lists are tested against
  bubblewrap's and mount(8)'s contracts, but they have not yet been proven on a real
  user-namespace host, so they are never chosen for you. In the namespace the
  configuration root, the state root, the Forgejo data root, your real home directory,
  `/tmp` and the engine's own temporary directory are replaced by empty mounts; only
  this run's own directory and its workspace are bound back. The run gets an empty
  `HOME` and its own `TMPDIR`; a sibling run's `tmp/askpass.toml` is behind the
  private `/tmp` and is not visible. The namespace's mounts are made private before
  anything is mounted, so an empty mount can never leak back to the host. Every path a
  mount is placed on is resolved with `realpath` first: bubblewrap refuses a symlink
  destination, and a symlink under `/tmp` whose target is elsewhere would otherwise
  survive the private `/tmp` with its target visible. The mounts stage this run's own
  files in `/dev/shm` (or `$XDG_RUNTIME_DIR`, or `/var/tmp`), mode 0700, removed when
  the run ends.
- **none**: this computer cannot run Landlock, the layout above cannot be confined, or
  the opted-in namespace does not work. An opted-in namespace that cannot be honoured
  gives **none**; the product never substitutes Landlock for the mechanism you asked
  for. The run is not confined, and the first-run
  screen and the board say `runs are not confined on this computer` in one sentence.
  Nothing is pretended. If a run's plan had promised a boundary and its run-time
  layout cannot keep it, the run records `none` in its plan, leaves a marker the
  engine puts on the board, and refuses to execute a command rather than run it
  unconfined behind the promise. A run whose Landlock probe fails at start takes the
  same path.

The run's environment is not inherited: it is cleared and replaced with a short
allow-list — `PATH`, `LANG` and `LC_*`, `TERM`, `TZ`, `HOME` and `TMPDIR` (the run's
own), and the `GEWU_*` and git variables the run needs. An exported `OPENAI_API_KEY`
or `AWS_SECRET_ACCESS_KEY` is therefore not readable inside the run.

What a confined run can still see on every mechanism: the system's files under `/etc`,
`/proc`, `/sys` and `/run`, the network, and the tools you installed. Its environment
is the short allow-list above, so a socket or an API key exported in any other variable
is not inherited; its stdin is empty. Under an opted-in bwrap or unshare it also sees
the rest of `/` except the replaced directories. It cannot read the product's
`credentials.toml` — the model key, the administrator token and the other agents'
logins — or `multiverse.toml`, the state root, the Forgejo data directory, another
agent's instruction, or another run's login file. A confined run's per-run askpass
file holds only **that run's own** Forgejo login and token, lives under the run's
private `TMPDIR`, and is removed when the run ends.

The key and every other secret in the credentials file are still redacted from every
string the run writes down — a tool's output, a model reply, an error message — before
it reaches the transcript or the log. That is a second line, not the boundary: a
command can still do anything the confinement above leaves open.

## What is not here

- **Streaming a reply.** The model exchange is one request and one reply. A streamed
  exchange is not here.
- **A scheduler.** A run starts when the agent is started, and the loop keeps it going
  until it is stopped. There is no timer and no cron.
- **A per-agent model or a second Forgejo.** One address, model and key serve every
  agent; one bundled Forgejo serves the installation.
