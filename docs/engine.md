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
    secrets/admin-login       0600; the login the instance was made with
    custom/templates/base/head_script.tmpl   the pinned page script plus the locale guard
    log/forgejo.log
    data/forgejo.db, data/repositories/
```

The data root comes from the platform's data directory: on Linux
`~/.local/share/gewu-multiverse` (or `$XDG_DATA_HOME/gewu-multiverse`).

The port is chosen once from 3000–3099 and then read back from `app.ini`, so a restart
uses the same address. SQLite runs in WAL mode with `SQLITE_TIMEOUT = 5000`, so two
agents writing at once wait for the lock instead of a 500. `migrate` runs before the
first administrative change; a new installation's administrator account uses the fixed
login `owner`, never the operating system account name. Forgejo reserves `admin` for
its own administration route, so `owner` is the fixed choice. An installation made by
an earlier version keeps the login recorded in `secrets/admin-login` and the page says
`(kept from an earlier setup)`.

The administrator password is never written down and never appears in a process
argument. The account is created with Forgejo's CLI `--random-password` (the value is
produced by the CLI, read only to be discarded, and wiped from the captured output);
`p` on the Forgejo page resets it through the administration API, with the new value in
the `PATCH` body, and puts it in the next view only. The consequence is a static line:
the password shown before that moment stops working.

The administrator API's repository list is polled every two seconds. A name that
disappeared or appeared between polls becomes an event on the board and on the Forgejo
page (`team/scratch was deleted`, `team/website was created`), so a destroyed
repository is never traceless. The newest event is written to the settings file and
loaded again on open, so it survives a reopen. The count and the event are replaced in
the same tick;
a failed poll is retried on the next tick instead of keeping an old count for another
interval (W31-8). The administrator's list includes private repositories, so a private
repository's deletion produces the same event. The process runs as `web` in
its own process group. It is ready when `/api/v1/version` answers.

Provisioning through the administration API is idempotent: organisation `team`, team
`agents` with write access to all its repositories (its units are code, issues, pull
requests and projects, all write, so Forgejo's own summary says `write`), and the
starter repository `team/workspace` initialised with a README. The pinned `app.ini`
also carries `[other] SHOW_FOOTER_TEMPLATE_LOAD_TIME = false`, and the same key is
added or replaced on an instance an earlier version made, so the page footer shows
`Powered by Forgejo` without the developer's page/template timings.

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

### Confining an agent's shell

An agent's shell is the one process a model can point anywhere. The **automatic**
mechanism is Landlock, the unprivileged kernel sandbox; the two namespace mechanisms
are kept but chosen **only when the person opts in**, and the method that really works
is written into every run's plan:

1. Landlock (automatic): the shell may read only its workspace, its private temporary
   directory (where its per-run askpass file lives), the system tools (`/usr`, `/bin`,
   `/sbin`, `/lib`, `/opt`), `/etc`, `/proc`, `/sys`, `/run`, `/dev` and the product
   binary's directory; the product's roots and the person's real home are denied,
   though their names still exist. The method is claimed only after a child process
   built a ruleset at `HardRequirement`, required `FullyEnforced`, read an allowed
   control file and was denied one outside the grant; a kernel without Landlock (or
   with it disabled) gives **none**, never a dummy ruleset. Landlock cannot subtract a
   rule from a granted tree, so this method is not chosen when one of those hidden
   roots lies inside a granted tree. A hidden root that resolves inside the workspace
   is beyond every mechanism, because Landlock grants the workspace and the namespaces
   bind it back; such a layout is **none**.
2. `bwrap`, a user and mount namespace, with `GEWU_CONFINEMENT=bwrap`: the
   configuration root, the state root, the Forgejo data root, the person's real home,
   `/tmp` and the engine's temporary root are replaced by empty mounts; only this
   run's own root and workspace are bound back, and the run gets an empty `HOME` and
   its own `TMPDIR`.
3. `unshare --user --map-root-user --mount --propagation private`, with
   `GEWU_CONFINEMENT=unshare`, the same picture built from `mount` calls. The script
   creates every bind target before the first bind, stages this run's own root and
   workspace outside the hides, then mounts a private `tmpfs` over `/tmp` and the
   engine's temporary root, hides the product's roots and the person's home, and binds
   back only this run's own paths. It also runs `mount --make-rprivate /` before its
   first mount: a scratch bind made inside the namespace must not be visible outside,
   and the capability probe checks exactly that before choosing this mechanism.
   Both namespace argument lists are tested against bubblewrap's and mount(8)'s
   contracts and with fake programs; they have not yet been proven on a real
   user-namespace host, which is why they are opt-in.

The engine chooses the method against this installation's own roots when it opens, and
enumerates every real `<agents>/<agent>/workspace` at that moment and again when an
agent is added. Every hidden path and the engine's own `TMPDIR` are resolved with
`realpath` before a mount is placed; a symlink cannot hide its target from the hide
list, and bubblewrap is never handed a symlink as a mount destination. A namespace
mechanism also needs a staging directory outside the hidden roots (usually `/dev/shm`);
without one it is not chosen, because it could not make `/tmp` private.

When Landlock is unavailable, the layout cannot be confined, or the opted-in namespace
does not work, the run is not confined and the board and the first-run warnings say
`runs are not confined on this computer`. An opted-in namespace that cannot be honoured
gives **none**; Landlock is never substituted for the mechanism the person asked for.
The product does not pretend. A run whose
plan had promised a boundary that its run-time layout cannot keep — or whose Landlock
probe fails when the run starts — records `none` in
its plan, leaves a marker the engine reads for the board, and does not execute a
command: a silent fallback to an open shell is never allowed.

What a run can still see under every mechanism: `/etc`, `/proc`, `/sys`, `/run`, the
network and the installed tools. Its environment is cleared to the short allow-list
described below, so a socket or a key the person exported is not inherited; its stdin
is empty. Under an opted-in bwrap or unshare it also sees the rest of the host root
outside the replaced directories. Under Landlock an attempt on a replaced directory
is `Permission denied`; under a namespace the name is an empty directory. Neither
mechanism hides the network, and neither is a container.

A confined shell gets an empty `HOME`, a private `TMPDIR`, an environment cleared to a
short allow-list, and a per-run askpass file under that `TMPDIR` holding only that run's own
Forgejo login and token, because git inside the run must authenticate. The run removes
its whole private temporary tree on every exit it sees (success, failure, a handled
stop); the engine removes the same tree by the exact pid + run id after a stopped,
killed or crashed run is reaped, and the next open sweeps a tree whose owner is gone.
An askpass file therefore cannot outlive its run. A sibling run's
askpass file is not visible: an opted-in namespace replaces the shared temporary tree
with a private `tmpfs`, and Landlock grants only this run's own subtree. The shared
`credentials.toml` — the model key, the administrator token and every other agent's
login — is outside the boundary. The run plan records the mechanism and the roots to
hide, so a run started by an older build stays unconfined rather than being
retroactively promised a boundary. A plan that is refused at run time is rewritten to
`none` and marked; the engine takes that refusal onto the board.

Landlock confines what a run can read and write, not processes. A command that
detaches itself — with `setsid`, `nohup` or `disown` — can outlive the run and the
product. Cleanup is best-effort: the engine stops the run's process group, but a
process that left the group is not reachable, and the person can see such a process
with `ps`.

### Left-over processes and the program's own ending

The running server writes its pid into `forgejo/instance/forgejo.pid`; each session
writes `session.pid` and the process group it leads into `session.pgid` in its run
directory. The run supervisor also writes the shell's own `shell.pid`, `shell.pgid` and
`shell.identity` as soon as each command's shell exists. The identity is the kernel boot
id and the process start time read from `/proc/<pid>/stat`, so a group is still named
after a supervisor is gone and its `session.*` records were removed. The supervisor
also creates an anonymous `memfd` ownership capability before the spawn and records
its device, inode and a random token in `shell.capability`; the shell and its children
inherit the descriptor, so the group is identifiable even before `shell.identity` is
written and while a child outlives the shell. The record is a description, not the
capability: an unrelated process can read or open the record path, but cannot acquire
the unnamed object, and an inode reused by another memfd fails the token check. The
files are removed when the process group is reaped. If the program is killed outright, the next
`open` walks every run directory and stops each recorded group, politely then by force,
before it opens. A stored number is never identity: each candidate is bound to a kernel
**pidfd** before its live boot id, start time and process group are read, and every TERM
and KILL is sent through that descriptor. Ownership is the inherited anonymous memory
file, not a named path. A pid that changes between the check and the
signal cannot be retargeted; there is no numeric `kill`/`killpg` on this path. If the
shell has died, the child processes observed under its live capability are remembered
by their own boot/start identity and only those exact pidfds are signalled. An arbitrary
argv path is not ownership: an unrelated `find <run_dir>` that reads the directory and
retains the path does not hold the run's capability. Before the first shell command,
the product supervisor is recognized by its executable and exact `--run-agent <run_dir>`
invocation; it has no shell capability yet. A bare `session.pid`, `session.pgid` or
`shell.pgid` is not identity. If a group is still live but cannot be proved this
run's — numbers-only old records, or a run directory whose records cannot be read — no
signal is sent and the failure is not hidden: the run records stay, the card carries the
reason, and the agent is blocked. Records that are only numbers are never enough to kill
an unrelated group; the safe direction is to report and block, not to discard and start a
replacement. A durable `shell.capability` record with no verified holder is start
evidence even when the transcript and numeric records are gone: on the current boot it
stays visible and blocks `Start`; a record from before this boot is closed with one final
`run_stopped`. A non-dumpable same-user holder whose `/proc/<pid>/fd` cannot be read is
that same uncertainty; unrelated inaccessible system processes are not a global blocker. A started run (its transcript has a record) whose process records are gone
is not cleared into a replacement either: it stays visible and blocking. A plan alone is
only a prepared run and may be cleared; a transcript that exists but cannot be read is a
failure. An unreadable state root, agent directory, run directory, record file or
`/proc` identity inspection is a failure, not "nothing alive"; a partial permission
(readable but not searchable, denied record metadata) is a failure too. A repaired agent
directory is re-enumerated and every run below it is checked before `Start` clears the
leftover. A recordless started run from before the current boot cannot have a live
process and receives one final `run_stopped` record instead of a permanent false block.
An empty selected target list never means "group empty": if any live member remains
unverified, the cleanup is visibly blocked and its evidence kept. No process environment
is read. A zombie is not counted as live, so a killed shell does not hold the recovery
open. At every run boundary and every reopen the order is fixed: stop or find the
group gone, ensure a durable `run_stopped` ending, and only then remove the run's
identity handles. A run that writes its own ending has no cleanup marker; when
recovery itself had to reap a left-over group or close a run that wrote none, it also
writes `run-stopped.json` with that ending. A failure at either write keeps the handles and is
reported, so a later `Start` retries instead of trusting an absent record. The same
ordering settles a normal `s` stop or a run ended by `q`; an interrupted program leaves
either handles that the next open can still verify or an ending it can trust. A run the
engine ended for its own reason also gets `run-stopped.json`: a normal stop, a deadline
timeout, and a deadline failure that keeps the harness's last reported cause. The marker
is written even when the reap cannot be verified, with that uncertainty in its sentence,
and the terminal ending waits for a verified reap. A run's own terminal record written
before a stop wins: the stop changes only the agent's desired state, writes no stop
marker and adds no second ending. A stop requested while the run had no ending wins;
if the child manages to write one during the grace window, the stop terminal replaces
it so the run still has exactly one terminal outcome. The first-ending fact is
write-once and is persisted at `first-ending.json` in the run's private directory
before the first stop, deadline or shutdown signal. A second Stop, a program shutdown,
settle, reap or reopen reads that file; it cannot re-sample the fact or hand the run
to a later child ending. Recovery uses it too: a persisted `stop` replaces a child's
post-stop ending with one `stopped` terminal plus the stop record, while a persisted
`terminal` keeps the child's ending and writes no stop record.

One owner in `run.rs` performs every ending operation: it pins the fact, sends the
polite/force signals, writes the stop and terminal records, settles and releases the
identity handles, performs reopen recovery, and records uncertainty. Engine callers
only request an ending (`prepare_ending`, `request_stop`, `request_force_stop`,
`stop_uncertain`, `request_stop_and_wait`, `request_finish`, `recover_ending`,
`request_natural_finish`, `request_failed_finish`. A prepared run is an owner
`run::ending::PreparedRun`: only `Owner::prepare` creates the directory, only
`start`/`cancel` consume it, and `Drop` records a prepared-run ending if neither did.
`request_cancel_prepared`);
they never write an ending record or signal a run handle directly.

The run process handle is an owner type with a private inner handle and a private
`signal` method. The owner consults the durable fact it returns: a natural or failed writer that sees
`stop` already won adds no terminal; only a `terminal` fact writes one. Uncertainty
evidence is installed with an atomic no-clobber link, so the first evidence survives
concurrent writers.

`Record::RunFinished` carries a private `EndingSeal` field; only the owner can make
one, so a terminal record cannot be constructed or appended outside `run.rs` even
through a type alias. `EndingSeal` deliberately has no `Default`; the only storage
path is `seal_from_storage`, used by serde when reading old records. `Record::run_finished` and `Record::record_outcome_if_missing`
require that seal. `tests/compile-fail.rs` compiles source probes against the library
and requires them to fail (type-alias construction, low-level names, force API).

`tests/ending-owner.rs` remains as the secondary whole-source scan. It covers every
production `src` file except `src/run.rs` (the owner) and `src/transcript.rs` (the
record storage primitive), strips tests and comments, and rejects typed terminal
constructions, literal ending file names, low-level settlement calls and raw run
signals. It also feeds deliberately bypassing source fixtures through the same check.

The fact is trusted only when it names this exact run and agrees with its records. A
missing/corrupt terminal under a persisted `terminal` fact, a foreign `run_id`, an
unreadable fact, or a fact that cannot be written before a governed signal is an
uncertain ending: the product writes `ending-uncertain.json` with the evidence, keeps
the handles and blocks dependent actions. It never falls back to a clean `stopped` or
`terminal` outcome. A cleanup stop that must still end a live child records that
uncertainty before the stop and does not settle the run cleanly.
The native supervisor writes `native-progress.json` whenever its per-harness outcome
model changes (waiting, retrying, failed, succeeded), so a cause the harness already
emitted survives the engine killing the child at its deadline. A failed tool is
persisted there as a warning, never as the session cause: a deadline with only a tool
warning stays `timed_out` and the stop record names the warning. Text is bound to the
attempt that produced it; a failed or retried attempt's partial text is never
committed by a later terminal success. If a
process survives the reap, the sentence is kept and drawn on the agent's card; a known
live process is `live leftover`, a live process whose ownership is unproven is
`unverified leftover`, and a run that left no ownership record (or whose record/ending
cannot be read) is `cleanup unverified`. The list form's compact words are `⚠ live`,
`⚠ unowned` and `⚠ unverified`, in the same order. The agent is marked blocked and the engine does not start a replacement
run beside it. `Start` retries the cleanup: if the old process is gone and the record
can be written, the leftover is cleared and the run may start; if not, the same reason
is reported.
A run agent also asks the kernel for `PR_SET_PDEATHSIG`, so a product killed with
`SIGKILL` ends its supervisor; the shell it started stays in the group and is reaped by
the next open. At each run boundary the engine reaps the finished run's group before the
next run is allowed to start, so a supervisor killed from outside cannot leave an old
command running beside the new one. Only after that recovery does the engine start a
fresh Forgejo, so two servers never share one data directory.

`q`, the terminal closing (`SIGHUP`) and the window manager ending the program
(`SIGTERM`) all take the same path: stop every session and Forgejo, leave the alternate
screen, and print one line when an agent was started (it resumes when the program is
opened again). A run ended by that path is settled like any other: its group is found
gone, its durable ending is written unless it has one, and only then are its identity
handles removed.

A quit while the Forgejo is still starting is handled too: the start job records the
child's pid immediately after spawning it, `shutdown` signals that pid as well as the
stored child, and it waits a bounded time for the start job to stop and reap its child.
The Forgejo access-token helper at the first `● running` frame is recorded the same way,
and its output is drained while it runs, so `shutdown` can end and reap it and a large
answer cannot stall the start.
A lost terminal — `SIGHUP`, `SIGTERM`, end-of-file, or `EIO` on a closed pseudo-terminal —
ends the input loop at once: `shutdown` runs once, the screen is not restored on a dead
terminal, and the program exits instead of retrying the dead input. An instruction-editor
or log-pager hand-off watches that same stop predicate: a caught signal or lost terminal
ends the hand-off program politely and then by force, reaps it, and returns so the board
runs that one `shutdown`; a normal hand-off still waits for the person's own program.

Closing the window leaves the old copy stopping its Forgejo and runs for a moment. A
reopen waits up to ten seconds for the data lock and then opens; if it is still held it
says `the previous copy is still stopping — try again in a moment` rather than a bare
exit 1.

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

`last-seen.toml` records the newest version that opened the configuration. It is
written at open and on a clean exit. When the recorded version is older than this
build, the first open shows one Upgrade screen: the version it was, the version it is,
how many agents, instructions and runs were found, what is reused, one line from this
release's notes naming what changed, that the old `config.toml` is ignored, and the
release page address. `terms_version` records the wording of the first-run warnings the
person agreed to; when a later build changes them, the upgrade screen says so and the
next screen shows the warnings again. After they are agreed the program returns to the
board through the same `after_upgrade()` choice a normal open uses — the first-run
model/picker flow is not entered and an existing installation's agents are never
re-created.

Builds before 0.1.31 wrote no marker, so a real upgrade from 0.1.30 has none. The
installer reads the installed binary's `--version` before replacing it and writes the
marker, which gives the exact old version. If a binary is replaced by other means, an
existing `multiverse.toml` (or the old text screens' `config.toml`) still gets the one
screen, with `this installation was an earlier version`; a fresh root gets no screen.
The engine writes the marker at open, so the next open goes straight to the board.
This is also why the marker is read *before* it is written in `upgrade_view` (W31-1).

The secrets are in `credentials.toml` beside it, created with mode 0600: the bundled
Forgejo's administrator token, the one model key (top-level `model_key`), and each
agent's own Forgejo token under `[forgejo_agent_tokens]`. A value is read only when it
is needed and is never printed.

## Agents and one model

`CreateAgents { count }` makes `agent-1` … `agent-N`; `AddAgent` makes a **new**
`agent-N` whose number this installation has never used. The high-water mark is kept
in `multiverse.toml` (`next_agent`) and raised by every name found in the settings,
the configuration directories and the state root, so a removed `agent-2` is never
handed out again even after an upgrade. Each name derives its Forgejo login
`gewu-<name>`: the account is created and marked, the token is made with repository,
user and issue rights, and the account joins team `agents`. Membership is idempotent.
Provisioning runs on a background thread and retries while Forgejo is up; the card
shows `Pending`, `Ready` or `Failed { reason }`.

`RemoveAgent` stops the agent, deletes its token on the server by the name recorded
when the token was made, removes it from team `agents`, deactivates the account
(`active = false` and `prohibit_login = true`, so a web sign-in is refused too),
deletes the dead token value from the credentials file, and forgets the agent in the
settings. The account, its commits and its run records stay; removal is not a history
rewrite. `AddAgent` never reuses that name: a new number gets a new login and a new
account, and the removed account stays deactivated with its history.

The model is one set of three facts — address, model, key — used by every agent. The
key is written to the shared top-level `model_key` in the credentials file.
`TestEndpoint` runs the model probe in the background and leaves its result in the
view. A failed test keeps the provider's whole message (bounded at 400 characters),
and the form wraps it to the rows it has; at 80×24 that is the whole answer, not its
first clause. A transport failure names the errno without the library's `os error`
gutter: `cannot reach the model: Connection refused (errno 111)`. The shorter lead-in
keeps the errno on the 72-column card, which has one row.

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

- succeeded, or ran at least 60 s: **5 s**, and the quick-failure streak resets;
- a successful run that found no work: the documented **20 s** watch interval, and the
  quick-failure streak resets too (a no-work success is not a failure);
- a warned run: the same **20 s** watch interval. A warning is not a verified success,
  so it never takes the five-second success loop that replayed a deterministic write
  every few seconds;
- otherwise: **10 s**, doubling with each consecutive quick failure, capped at
  **5 min** (10, 20, 40, 80, 160, 300, 300 …).

`Stop` sends the existing polite `SIGTERM` to the run's process group; after the grace
the engine sends `SIGKILL`. It clears any scheduled run, so nothing new starts. A run
that overruns the session timeout is stopped the same way and recorded as `TimedOut`.

`runs_total` is every run record on disk plus every run started while the engine is
open; it is what the card's `run N` shows and it never resets on Start or reopen
(W31-5, F5). `recent` is the newest ten endings, reconstructed from the run records on
open and updated as runs end. `activity` is the newest Forgejo action of the newest run, read from
that run's transcript; after the run ends it is drawn as `last: … · 2m ago`, shortened
to the compact action and `2m` only where a roomy card is narrow. The run
writes `current-tool` while a tool call is in flight (so the card can say `shell · 12s`)
and `last-failure` when one fails (so the card carries the failed command's cause in
its row: `⚠ <tool>: <first clause>` for a warned run, `✗` for a failed one; the log
keeps `✗`). `last_work` is the issue, pull request, branch and comment
the newest run named, read from its transcript. A pull request's branch comes from
that request's own tool result; a branch-only run uses the push witness. A legacy
request with no recorded branch keeps its number and makes no branch claim.

A clean exit is not automatically `✓`. The mark reads the shell's raw record: it is
`✓` only when every status the shell reported — each trap record's `$?` and every
element of every `PIPESTATUS` — is 0, and the wrapper exited 0. Pairing only supplies
the words and the `⊘`/`⚠` distinction; a non-zero raw status the pairing could not
attach to a command makes the run `⚠`, never `✓`.

The run's own tool results are read first: a refused Forgejo write, or a shell write
that fails, marks the run `refused` even when the model's last sentence is phrased
positively. The shell reports every simple command it completes to the product: the
command runs under `bash` (when the host has it) with a `DEBUG` trap installed under
`set -T`, so subshells, command substitutions and functions are covered; an inherited
no-op `ERR` trap under `set -E` also makes a failing last command inside a subshell or
command substitution report itself. The recorder is installed by a product-owned
`BASH_ENV` prelude, so a nested non-interactive `bash` that reads `BASH_ENV` — `bash
-c …`, a `bash script.sh`, a `#!/bin/bash` script — installs the same recorder in the
same file; a nested shell marks its own frame with `NEST`/`NESTSTART`/`NESTEND`, and
its records are paired inside that frame. Each trap record carries the status of the command that just
ended and `PIPESTATUS`; for a one-element record a non-zero `PIPESTATUS[0]` wins, so
the failing condition of `if`/`while`/`until`/`!` is not lost to the enclosing
compound's 0. A pipeline record's `PIPESTATUS` is paired inside that shell's frame, fills an
unpaired command, and never replaces a non-zero status already paired with a command; a duplicate
record from the inherited `ERR` trap is collapsed to one segment. The trap body
captures those values as arguments before turning `set -x` off for its own line and
restoring it, so a model's `set -x` shows the recorder's call but not the record's
format. The records go to a private file under the run's 0700 temporary root (or its
run directory when the shell is unconfined), never to the model's stdout or stderr,
and the tool result carries them as `segments: [{command, status}]`. Any non-zero raw
status makes the run `warned` (`⚠`) with the failed command's cause as its row; a
failed command that is a recognised write makes it `refused` (`⊘`), whatever the
model says. When the pairing cannot name the failed command, the raw status's output
line is the `⚠` cause, and the run is never `✓`. A write whose enclosing compound
hides its status — `if rm …; then …; fi | cat` — is `⊘` when the operating system's
own diagnostic is in the output; any program's POSIX diagnostic
(`<program-or-shell>: [line N: ][<path>: ]<strerror>`, ending in any error message the
C library itself prints — `Permission denied`, `Read-only file system`, `File name too
long`, `Too many levels of symbolic links` and every other errno text — or
`cannot create`/`cannot remove`/`cannot open`/`cannot stat`/`cannot access` naming a
path) is a witness, so an unlisted program's failure there is `⚠`. When a mixed
failure prints more than one diagnostic, the failed write's own line is the cause
row. A long POSIX diagnostic is shortened to `<program>: …: <error>` so the error words
survive a card's one-line cut; the transcript keeps the full line. A failed
`[ … ]`/`test` condition names its status, never a later stdout line. The shell's own
voice is the third witness. Every shell that reads the prelude
reports its `$0` — the outer shell and every nested `bash` that reads `BASH_ENV` —
and the trap reports `BASH_SOURCE[0]`, the name bash uses for a sourced command or a
function defined there; bash prefixes its own errors and warnings with that exact
`<name>: ` (`<name>: line
N: <message>` and `<name>: <message>`), and a function defined on the `-c` line is
named `environment`. Any output line in that form is shell-reported trouble whatever
the text says, so no message list is kept: `ambiguous redirect`, `cannot overwrite
existing file` and every other shell wording are seen. An all-zero raw record then
makes the run `⚠`, or `⊘` when the command names a recognised write or when the
shell's record names a write whose plain target the diagnostic carries; the rule can
only lower the mark. A failure that leaves neither a status nor any diagnostic text
the system or the shell prints is limit (f) or (g) below. A write that exits non-zero
is a refusal whatever its output says. A failed
Forgejo read is also a failed call and marks the run `warned`, so "every tool call" is
true as written. On a host without `bash` the shell is `/bin/sh`; when the private
record cannot be opened the run continues without it. In both cases the product falls
back to reading the output: a compound command is split on `;`, `&&`, `||`, `|` and
newlines outside quotes, and a write segment plus a failure-context line (`fatal:`,
`error:`, `remote rejected`, `failed to push`, `Permission denied`, `denied`, an HTTP
4xx/5xx status line, `curl: (22)`) is a refusal even when the wrapper exits 0; the run
log says the recorder is off. A `curl` without `-f` exits 0 on a 4xx/5xx, so that one
call's output is also read for a status line or an HTTP error phrase; a write that
succeeded is not otherwise re-read for words. Seven limits are stated as built: **(a)**
a model that forges or truncates the record file, closes the recorder (unsets or
overrides `BASH_ENV`), masks a failure in a tool that prints nothing, or suppresses
both the status and the output of a write hidden inside a compound, defeats the mark
(the transcript and `l` keep what the tool printed, which may be nothing); **(b)** a `curl` without `-f` whose 200 body
literally says `Forbidden` is called refused — the safe direction, and contrived;
**(c)** the rule is deliberately conservative and only lowers the mark: a condition
that failed and was recovered from, a system error string printed by a command
that did not fail, or a line that merely starts with a shell name may read `⚠`
(never the reverse); **(d)** on a host without
`bash` the shell is `/bin/sh` and the product falls back to
the output heuristics above (the run log says the recorder is off); **(e)** a
nested interpreter that does not load the product's recorder — any non-bash
interpreter (`sh -c` when `/bin/sh` is not bash, `python -c`, `perl -e`), or `bash`
in POSIX or privileged mode (`--posix`, `-p`, `POSIXLY_CORRECT=1`) — runs outside the
record and its failed write is seen only through the outer command's exit status and
the output heuristics; **(f)** a failure that
leaves neither a raw status nor any diagnostic text — a program that fails silently
inside a piped condition — cannot be seen; and **(g)** a failure inside a piped or
negated condition reported only by a program's own free-form message (neither a
system error string nor a line in the shell's own prefix), with an all-zero raw
record, cannot be seen. In (f) and (g) the transcript and `l` keep what there was.
Any other failed tool call — a non-zero
exit or a signal, not a recognised write, with no refusal evidence in its output —
marks the run `warned` (`⚠`), with the failed command's cause as its row; it is never
`✓`. A failed **read** is in this class even when its output carries a 403 or
`Permission denied`: `⊘` is only a refused write (F-02). A successful
`forgejo_list_assigned_issues` that returned no issues is the tool-result witness for
`no work`: if nothing was written, the run is `∅` even when the model's own sentence
says everything is done (U3). A run that only read its issue and wrote nothing is
also `∅` with its own sentence, whatever change word its instruction used. A change
left only in the workspace — content, mode, type, a link, a directory, a commit, or a
local tag, index, config or ref — is `⚠`, and its reason and work line name the
recorded local work. A fetch that only moves remote-tracking refs, their logs or
`FETCH_HEAD` is reading, not local work. A run that recorded no tool call at all is
never `✓`; its requested work is unconfirmed. Declared steps and native tool results
are the same transcript facts as model tools. A real remote write — a verified push, an opened or updated
pull request, or a comment — outranks a later refused write or failed command and
keeps `✓`; the work block names the request or comment, not only the branch.
Otherwise the last sentence decides between `refused` (when it names a write or the
instruction was destructive), `no work`, `warned` and `succeeded`; a review that
found no problem and says the work is complete stays a success. A refused write's
cause row says `write refused: …` and wears `⊘` on the card, matching the strip. A destructive instruction answered only in prose (`I left it
in place`, `I did not delete it`, `deleting it is not safe`) is also a refusal,
because there is no tool result to read and the run's own sentence says the work was
not done; a keep phrase beside a success verb for the destructive act stays a success.
The final sentence is kept as the terminal record's reason for every clean finish, so
the card can draw it beside `✓` as well as `⊘`/`∅`/`⚠`. The card's summary line
prefers the newest finished run that did work (`last work: …`); a later check run's
reason is labelled `no work: …`, so a no-op never stands for the agent's whole work.
The run log writes a newline after a tool's output even when the tool's last line had
none, so the next stamped line never joins itself to a URL.
An instruction's stored first line is cut at 120 characters **on a word boundary**, so
the card never renders `extraord…` from a word the engine itself halved.

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

The terms the board shows are the five fixed plain sentences in `engine::TERMS`, plus
the one-sentence confinement warning when the host offers no confinement; the
mechanism names (Landlock, bwrap, unshare) are in this document and the shell tool's
description, not on the consent screen. The model test distinguishes its own suffix
and empty-key checks (nothing sent) from a provider refusal (status and the provider's
own message, kept whole and wrapped to the form's rows at 80 columns); a 404 on a
`/chat/completions` path names the `/v1` form first. The
engine's reasons, notices and model-test lines are written in the same plain words the
board uses. `src/board/status.rs` is the one cleaner for engine-authored text and also
removes the backticks the engine writes around names. It is deliberately not applied to
a run's own final summary: the model's words are not rewritten apart from the existing
credential masking, so an ordinary word such as `missing` inside a sentence is never
turned into a status label. A long summary may be shortened on the visible row with an
explicit `…`; the transcript (`t`) keeps the whole sentence.
