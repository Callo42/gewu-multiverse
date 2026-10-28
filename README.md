# gewu-multiverse

**One Forgejo on your own computer, and the agents you run on it — drawn as one picture in your terminal.**

You install one small program. It brings its own Forgejo and keeps it running for you.
You choose a model, choose how many agents you want, and write each one an instruction.
Every agent has its own login on your Forgejo, so its commits, comments and pull
requests are its own. The board shows the whole thing at a glance:

```
╭─╴ gewu-multiverse ╶──────────────────────────────────────────────────────────────────────────────╮
│                                                                                                  │
│                                                                                                  │
│                       ╭╴ FORGEJO ╶─────────────────────────────╴ ● running ╶─╮                   │
│                       │                                                      │                   │
│                       │ http://127.0.0.1:3000                                │                   │
│                       │ team · 1 repository · team/workspace                 │                   │
│                       │ 3 agents                                             │                   │
│                       │ your login: owner                                    │                   │
│                       ╰───────────────────────────┬──────────────────────────╯                   │
│                                                   │                                              │
│                  ╭────────────────────────────────┼────────────────────────────────╮             │
│ ╭╴ ○ agent-1 ╶───┴───────────╮   ╭╴ ○ agent-2 ╶───┴───────────╮   ╭╴ ○ agent-3 ╶───┴───────────╮ │
│ │ gewu-agent-1               │   │ gewu-agent-2               │   │ gewu-agent-3               │ │
│ │ stopped                    │   │ stopped                    │   │ stopped                    │ │
│ │                            │   │                            │   │                            │ │
│ ╰────────────────────────────╯   ╰────────────────────────────╯   ╰────────────────────────────╯ │
│                                                                                                  │
│ ←→ pick   ⏎ open   s start/stop   + add   − remove                                               │
│ f Forgejo   m model   ? help   q quit                                                            │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
```

## What it is

- **One Forgejo, on your machine.** The program downloads a fixed Forgejo release,
  starts it, and gives you an administrator login. Nothing is hosted anywhere else,
  and the address is reachable only from this computer.
- **Agents with their own accounts.** Each agent is a Forgejo account named
  `gewu-<agent>`. When it commits, comments or opens a pull request, Forgejo shows the
  agent as the author, not you.
- **A real agent program per agent.** A fresh install brings Pi 1.4.0. On an
  agent's Program screen (`p`) you can install and select Codex 0.164.0, DeepSeek
  Harness 0.2.1-rc.2 or OpenCode 2.1.4, each from its official pinned package. The
  program owns its model turns, tools, retries, compaction and session; the product
  owns the run boundary, the Forgejo tools and the record. If Codex's own sandbox
  cannot start on this computer, the Agent screen keeps the cause and names the
  double-`f` opt-out; pressing `f` twice there performs it and the product Landlock
  boundary stays. A missing native-program prerequisite names `i`, and pressing it on
  that screen starts the install.
- **One default connection, and a saved choice per agent.** The first-run model
  address and key are the default. Each agent can save its own compatible
  provider/protocol/address/model/key; the protocol is shown beside the address and
  model, a save is used by that agent's next run, while the running run keeps the exact
  facts it started with. The board and the agent
  screen show active and next when they differ.
- **Migrate only when you choose.** An agent made by an older installation keeps the
  built-in engine. Its instruction, workspace, steps, skills, account and records are
  untouched until you deliberately choose another program.
- **A card for each agent.** A filled circle means working, a half circle means waiting
  for its next run, an empty ring means stopped or not started. The board card keeps
  the few words: the agent's name, its program (`built-in`, `Pi`, `Codex`, `DSH` or
  `OpenCode`), its Forgejo login, its state, its newest run mark and its run count. `run N` is the total from the run records, so it does not reset when an
  agent starts again or the program reopens. Open it with `⏎` for the Agent screen:
  the last work block (typed, and kept until a later run does new work), the
  instruction's first line, always, and `… (N more lines)` where N counts the
  document's **file** lines (not wrapped display rows); where no count row fits, the
  visible cut itself ends in `…`, the bounded run marks with
  their legend (`✓ ok ⊘ refused ∅ no work ✗ failed ⚠ cmd ■ stopped ◷ timed out`,
  wrapped whole on two rows at 72 columns), the newest mark's word, the last
  tool failure and the newest run's reason. The run's own final sentence is not
  rewritten apart from the credential masking; when it is too long for the visible
  row it is shortened with an explicit `…`, and `t` opens the transcript that keeps
  the whole sentence. The product's status vocabulary is never substituted into it. `✓` means every status the shell itself reported was 0: each trap record's `$?` and
  every element of `PIPESTATUS`, so a failed command in a pipeline, a condition or a
  nested `bash -c`/`#!/bin/bash` script that reads the prelude can never hide behind
  a zero-exit wrapper, and
  no line starts with the shell's own `<name>:` diagnostic prefix. Any
  failed status is `⚠` with its cause as the row, or `⊘ refused` when it was a write; a
  write inside a compound whose status the shell hides is `⊘` when the operating
  system's own diagnostic is in the output, when the shell itself prints a line in its
  own `<name>:` prefix (the shell's own name, the name of a nested `bash` that reads
  `BASH_ENV`, or a sourced file's —
  whatever the message — `ambiguous redirect`, `cannot overwrite
  existing file`, …, or a recorded write's plain target named by the line), or when
  any program's diagnostic is read — any
  system error message the C library prints, not a list of programs or a list of error
  strings. (Limits **(a)**–**(g)** in `docs/board.md`: a model that forges
  the record or masks a failure, a `curl` without `-f` whose 200 body says `Forbidden`,
  the conservative rule that only ever lowers the mark, a host without `bash`, a
  nested interpreter that does not load the recorder (a non-bash interpreter, or
  `bash` in POSIX or privileged mode), a failure that leaves neither a status nor any
  diagnostic text, and a failure inside a piped or negated condition reported only by
  a program's own free-form message.) A run that exited 0 with a refused or failed
  write and no other work is `⊘ refused` (or `⚠` when the failed call is a read);
  the refused write's cause row says `⊘ write refused: …`, matching the strip. A run
  that only read its issue and wrote nothing is `∅ no work` with its own sentence,
  even when its instruction used a change word; the same is true when its
  assigned-issues list came back empty. A change left only in the workspace — a
  content edit, a mode change, a new link, an empty directory, or a local commit or
  Git ref change — is `⚠`, and its work line names the recorded local work
  (`local change README.md`) instead of borrowing an older run's branch. A run that
  recorded no tool call at all is never `✓`: its requested work is unconfirmed. A run
  that actually pushed, opened or updated a pull request, or commented stays `✓` even
  if a later write was refused, a test failed first, it reused the branch an
  interrupted run left, or it ended with a sentence claiming failure; the work block
  keeps the product-observed fact, including the pull request. The
  summary line prefers the newest run that did work (`last work: comment on #2`,
  `last work: PR #3 open`, `last work: local change README.md`); a later no-op's
  reason is labelled `no work: …`. Started agents keep
  a visible idle between runs (the Agent screen says `next run in 5s`; a card's shorter
  form is `next 5s`); a no-work or warned run's longer wait says
  `next run in 20s · no work` (`next 20s·no work` on a card). A 503 is named as
  temporary and says it is retrying. The selected card is marked `▸`, and
  a window from 140 to 160 columns uses its width for the repository names and card
  detail; a wider one centres the same picture in 160.

The program keeps the Forgejo running while it is open. When an agent is started, it
runs its instruction, and when that run ends it starts another one after a short
visible idle, until you stop it. Each run's process group and the shell's boot/start
identity are written down; if the program or a run's supervisor is killed outright, the
next start checks the live process identity, binds each process to a kernel pidfd, stops
the group through those handles, and writes the run's durable ending (and a
`run_stopped` cleanup record when it had to close the run) before the board opens, so a
shell cannot keep editing beside the new run. A process the engine still cannot stop is named on the agent's card,
and the agent stays blocked until `Start` retries the cleanup successfully. An agent's
shell is confined where the computer allows it. **Landlock is the automatic mechanism**:
it denies the product's roots, and is not chosen when one of them lies inside a tree it
must allow. The product claims Landlock
only after a child process proved that a ruleset is fully enforced — it reads a file it
allows and is denied one it does not — so a kernel without Landlock gets the `none`
sentence below, not a promise. **bwrap or unshare are
opt-in** (`GEWU_CONFINEMENT=bwrap` or `GEWU_CONFINEMENT=unshare`); they replace this
product's configuration, state and Forgejo data roots, your home, and the engine's
temporary directory (with `/tmp`) with empty mounts, bind back only this run's own
directory and workspace, and clear the run's environment to a short allow-list. Their
complete argument lists are tested against the real contracts but they are not yet
proven on a real user-namespace host, so they are never chosen for you; an opt-in that
cannot be honoured gives `none`, never a substitute mechanism. Every hidden
path and the engine's temporary root are resolved with `realpath` first, and the engine
enumerates every real workspace, so a symlinked root cannot slip past. The workspace,
the run's private `TMPDIR`, the network and the tools you installed stay visible, as do
the system's `/etc`, `/proc`, `/sys` and `/run`. The warnings screen states what is true on **this** computer: `runs on this computer
are confined (Landlock)` (or `bwrap`/`unshare`) after the second warning, or the same
negative line when the computer offers no mechanism or the layout cannot be confined.
The warnings are one compact numbered block: five warnings, or six when this computer
cannot confine a run; the footer states the count, and the fact about this computer is
the first line under item 2.
The board repeats the negative sentence. One layout rule holds for 1–12 agents at
72×18, 80×24, 100×30, 120×40 and 200×60 (a home past twelve keeps the compact
two-column list and scrolls with the selection): cards never collide, truncate a name or leave
a dangling connector; the grid keeps its columns fixed so a wrapped last row cannot
move under the wire. When the cards cannot keep a whole title and their facts, the
board takes a compact list: one wired box with a line (or two columns) per agent,
keeping the name, run count, state mark and the current or last-work word on the
frame. The login is on the line while it fits; otherwise the box legend
`login=gewu-<name>` names the exact generated login when that equality holds (the
Agent screen always shows the whole login); a persisted non-standard login stays
whole and agent-associated. On a mixed compact board, an exact `agent-N` name
displays as `N` under the same-frame rule `N=agent-N`, followed by the actual
`N:<login>`, so a third, fourth or later recorded identity needs no separate map and
cannot lose its row association. Names that do not equal `agent-N` stay verbatim;
generated rows keep their inline login or a `*` marker covered by `*=gewu-<name>`
only when that equality is true. The
72×18 Forgejo box keeps the address, repository count, agent count and login on three
rows (the version stays on the top border); the 80×24 box keeps the organisation, the
repository names that fit and the login on four rows; from 100×30 the box is six rows.
The version and then the names are the first facts given up when the box is tight. The newest repository event is stored
with the settings, so it survives a reopen and has a footer row of its own when the box
is too short for it, never overwriting the keys.

The last-work word keeps its number: `pull request #100` is `#100`, never `#10`, and
`#10000` stays whole — the label is shortened, not the number. A failed run's word
starts with the HTTP status (`✗ HTTP 500` on a card; the Agent screen names the model
as well) and is cut on whole words, and
a single-column connector is placed after the label so it never crosses the word.

After an upgrade, the first open shows one screen: the version this installation was,
the version this is, what was found (settings, agents, instructions and runs), what is
reused or ignored, one line from this release's notes naming what changed, and the
release page address for this exact build (`…/releases/tag/v0.1.39`). If the first-run warnings changed since this installation last
agreed to them, the screen says so and the next screen shows them again; after they are
agreed the program returns to the board, never into the first-run model and picker
flow. The next open goes straight to the board. The installer records the old version before it replaces the binary, and a
home whose old build wrote no version marker (0.1.30 and earlier) still gets the
screen, naming the version as an earlier one when it cannot know it exactly.

## Install

One line, no Rust:

```sh
curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
# Pi is installed by default. Other official programs are explicit:
sh install.sh --harness pi,codex
sh install.sh --harness none        # no agent program; a new agent still starts as Pi
```

`--harness none` installs no native agent program. A fresh installation still
defaults a new agent to Pi, which is not installed: its card says Pi is not
installed until you choose the built-in engine there or install Pi.

The one-liner trusts the TLS connection to GitHub and the repository's release page —
the checksum detects a damaged download, not a replaced release — and the bundled
Forgejo binary is additionally pinned by digest and signed upstream.

It downloads the current prebuilt program to `~/.local/bin/gewu-multiverse`, the
Forgejo it needs (about 122 MB), and the one agent program you named (Pi by default)
to `~/.local/share/gewu-multiverse/`. Every package is checked against its published
SHA-256 or npm integrity. DeepSeek Harness needs Node 24.9 or newer and npm 11.16.0
or newer (older npm ignores the reviewed install-script policy); its locked closure
is about 613 packages and 753 MB. Nothing else is downloaded unless
you name it. If `~/.local/bin`
is not on your `PATH`, the installer says how to add it. The prebuilt program is for
64-bit x86 Linux with glibc 2.39 or newer (for example Ubuntu 24.04 or newer).

From source, with Rust 1.85 or newer:

```sh
cargo install --git https://github.com/Callo42/gewu-multiverse
```

Then run:

```sh
gewu-multiverse
```

## The four steps

On the first start the board walks you through these; after that it opens on the board
and you can change any of them later.

1. **Read the warnings.** They say that agents use this computer with your
   permissions. When this computer can confine an agent's shell, a run cannot read your
   configuration, saved keys, Forgejo data, home or another run's login file; when it
   cannot, the same screen says, in one sentence,
   `runs are not confined on this computer`; on a host that can confine, it says
   `runs on this computer are confined (Landlock)` under the second warning. The screen
   is one numbered block and its footer states how many warnings there are. Landlock is the automatic
   mechanism; `bwrap` and `unshare` are opt-in with
   `GEWU_CONFINEMENT=bwrap` or `GEWU_CONFINEMENT=unshare` and are tested but not yet
   proven on a real user-namespace host. Press `y` after you have read every one.
2. **Set the model.** Give the full model address (it ends in `/chat/completions`), the
   model name, and the key. Press `t` to test it; `✓ the model answered` means it
   works. The address suffix and an empty key are checked before anything is sent;
   on a per-agent form `ctrl-u` then `⏎` clears that agent's stored key;
   every other failure names the provider's status and its own message, kept whole and
   wrapped to the form's rows at 80 columns (a 404 on a `/chat/completions` address
   without `/v1` says so first). `↑`/`↓` moves between fields; `⏎` keeps a field and
   selects the next one without entering its edit mode, so `t` still tests; `Ctrl-U`
   (shown as `ctrl-u clear`) empties the selected field. Continuing past the model step
   runs the same address and key check: a wrong address stays on the form with the `✗`
   line, and a valid but untested one says `not tested — t to test`. The saved and the
   edited key are both shown as eight bullets, and a test result disappears on the
   first edit.
3. **Choose how many agents.** The first setup offers 1 to 12: `←` / `→` changes the
   number. Press `⏎` to create them. Later, `+` adds one agent at a time and an
   installation can grow to 20 in all; `−` removes the selected one. The board shows
   `downloading n%`, `setting up`, `starting`, then `● running` while your Forgejo is
   prepared.
4. **Write each agent's instruction.** Pick a card with `←` / `→` and press `⏎`, then
   `e`. Your own editor opens `instruction.md`; write what the agent should do and
   save. Press `s` to start it. Press `s` again to stop it. **Assign an issue in
   Forgejo first:** the assigned-issues list is the starting point. An unassigned
   issue is not listed, though an agent can still read or comment on an issue in a
   cloned team repository with its own account.

After the first setup, `+` adds one new agent (the settings' twenty is the most an
installation can hold) and `−` removes the selected one (it asks once). A removed agent's number and Forgejo account are never reused: `+` after
removing `agent-2` makes `agent-3`, with its own new account, while the removed account
stays deactivated with its history. The last agent cannot be removed and the screen says
why. `m` opens the model settings, `f` opens the Forgejo page, `?` lists every key, and
`q` quits.

## Where things live

| What | Where |
|---|---|
| The program | `~/.local/bin/gewu-multiverse` |
| Your settings (model address and name, Forgejo login, agent names) | `~/.config/gewu-multiverse/multiverse.toml` |
| Your saved keys | `~/.config/gewu-multiverse/credentials.toml` (mode 0600, yours alone) |
| Each agent's instruction, skills and workspace | `~/.config/gewu-multiverse/agents/<agent>/` |
| Every run's log and record | `~/.local/state/gewu-multiverse/<agent>/<run>/` |
| Forgejo and its database | `~/.local/share/gewu-multiverse/forgejo/` |
| The Forgejo administrator password | not on disk: `p` on the Forgejo page resets it through Forgejo's administration API and shows the new one |

The Forgejo address is on the board and on the Forgejo page, for example
`http://127.0.0.1:3000`. The page also says `assigned issues are the focus; comments
in cloned team repos`, and its organisation row says `team — create repositories under
team`; its team link is `/org/<organisation>/teams/<team>`. On the Forgejo page (`f`) your login is shown next to
`your login:`, the repositories are listed by name, and the agents' one-line
permission summary is there with the team page address. The newest repository event is
on the page and on the board at every supported size; the 80-column board also keeps
the organisation, the repository count and your login. A new installation uses the
fixed login `owner`, not your computer account name; an installation made by an
earlier version keeps its recorded login, and the page says `(kept from an earlier
setup)`.

The password is never written down. Open the Forgejo page and press `p`: the product
resets it through Forgejo's administration API, with the new value in the request body,
and the new value is shown, wrapped in full, for ten seconds — deliberately two lines
at 80 columns, never cut. `c` puts the shown value on the terminal clipboard with one
OSC 52 sequence and shows `copied`; a terminal without OSC 52 ignores the sequence and
the value is still on screen. The one-line consequence is a static row on the page:
`p resets it, c copies — shown for 10 s, old stops`, so the password shown before that
moment no longer works. While the value is on show, `c` puts it on the terminal's
clipboard with one OSC 52 sequence (base64) — never into a file, a log or a second
plaintext line. A terminal without OSC 52 ignores the sequence, and the screen still
shows the value for its ten seconds.

`--config-dir <dir>` moves the settings, keys and agent folders; `--state-dir <dir>` moves the run records.
A relative folder is taken from the folder you started the program in.
`--demo` opens the board with a scripted example and no real Forgejo.

## Stopping, quitting, and opening again

- `s` stops the selected agent. Its current run is asked to stop, and then made to stop
  if it does not; the run's ending is written to its record before the process handles
  are removed, and nothing new starts.
- `q` quits. If an agent is running, the board first names the agents that will stop —
  `quit stops agent-2 and agent-3, and Forgejo; they start again on reopen — q again
  quits`; press `q` again. An agent that is only
  started and blocked on an instruction is not named, because it has no live run. It
  stops every live run and the Forgejo, then prints one line counting the same live
  agents the warning named (`stopped 1 agent`, not `1 agents`); the started choices
  still resume when you reopen.
  Closing the terminal window does the same. A run ended by quitting is settled like a
  stopped one: its durable ending is written before its process handles are removed.
- Opening the program again starts the Forgejo, and starts the agents you had left
  started, with the same instructions.
- If the program was killed outright, the next start finds the Forgejo and any run
  process groups it left behind, stops them, writes the durable ending for each run it
  has to close (with `run-stopped.json` where it reaps a group or supplies a missing
  ending), and only then opens. A group is believed only through live identity: the shell's recorded
  boot id and start time checked against the live process, or the inherited anonymous
  capability, or the actual supervisor executable with its exact run invocation.
  A run-directory path in some other process's argv does not own the group;
  stored pid/group numbers alone are never trusted; no other process's environment is
  read. A process that survives, or a live group the
  engine cannot prove is this run's, is named on the agent's card and blocks a replacement
  run until `Start` retries the cleanup. A stopped, killed
  or crashed run's private temporary directory — including the per-run `askpass.toml` that held its own
  login — is removed by the run on every exit it sees and by the engine or the next open
  otherwise; a live run's directory is never touched.

## Removing it

The program is one file; removing it is the whole uninstall:

```sh
rm ~/.local/bin/gewu-multiverse
```

Your data is in three folders. Removing them removes the Forgejo, its database, your
settings, your agents' files and every run record:

```sh
rm -rf ~/.config/gewu-multiverse ~/.local/state/gewu-multiverse ~/.local/share/gewu-multiverse
```

Nothing else is installed: no service, no system-wide file, no other account.

## When something goes wrong

- **The Forgejo download did not finish.** The board says
  `could not download Forgejo — check the internet connection; the next start tries again`.
  Check the connection and start the program again, or run
  `gewu-multiverse --install-forgejo` and watch the percentage. Nothing partly
  downloaded is kept.
- **The model test fails.** The board gives one line. `✗ wrong address — it should end
  in /chat/completions` and `✗ the API key is empty` mean nothing was sent. Any other
  line is the provider's answer: `✗ the provider refused the API key (HTTP 401)`
  (check the key), `✗ the provider denied access to this model (HTTP 403)` (check the
  model and its access), `✗ no model called <name> at this address (HTTP 404)` (check the
  model name), `✗ the provider answered HTTP 404: …` (check the address; on a
  `/chat/completions` path without `/v1`, add `/v1`), or `✗ no answer within 15 s`
  (the provider did not answer; try again).
- **An agent keeps failing.** Its card shows `next run in Ns` and its last runs as
  `✓`, `⊘`, `∅`, `⚠`, `✗`, `■` and `◷`. Open it with `⏎`; `l` opens the newest log,
  `t` a readable transcript (turns, tools and their output) and `j` the raw JSONL
  record, in your pager. The wait between attempts starts at 10 seconds and doubles up to 5
  minutes, so a broken instruction does not spin. Fix the instruction with `e` or the
  model with `m`, then stop and start the agent with `s`.
- **Forgejo stops unexpectedly.** The board says
  `Forgejo stopped unexpectedly — restarting it` and tries again, waiting a little longer
  each time. If it says `stopped: …` while it is still setting up, press `r` to try now.
- **Another copy is already open.** The program uses one data folder at a time and says
  so; close the other copy first.

## What it does not do

- It is **not a container and not a privilege boundary**. Agents run as you, with your
  permissions, and their shell is confined only by the mechanism this computer offers.
  **Landlock is chosen automatically.** `bwrap` and `unshare` are chosen only when you
  ask for one (`GEWU_CONFINEMENT=bwrap` or `GEWU_CONFINEMENT=unshare`); their complete
  argument lists are tested against the real contracts, but they are not yet proven on
  a real user-namespace host. A confined run cannot read the product's configuration,
  saved keys, state or Forgejo data root, your home, another run's login file, or a
  secret you exported in an ordinary variable such as `OPENAI_API_KEY` (the run starts
  with a cleared environment: only `PATH`, `LANG`/`LC_*`, `TERM` and `TZ` survive).
  Landlock denies the product's roots and is skipped when one lies inside a tree it
  must allow; it is claimed only after a child probe proved a fully enforced ruleset
  (a denied read outside the grant). An opted-in namespace makes `/tmp` and the
  engine's temporary directory private; an opt-in that cannot be honoured gives
  `none` rather than Landlock. Every hidden path and the engine's temporary root are
  resolved with `realpath` first, and the engine enumerates every real agent
  workspace, so a symlinked root cannot slip past the check. A confined run can still
  read `/etc`, `/proc`, `/sys` and `/run`, reach the network and use the tools you
  installed. When no mechanism works, the run is not confined at all and the board
  says so; a run whose plan promised a boundary it cannot keep records `none` and
  does not execute the command. The program also removes saved keys from the logs and
  records it writes, but redaction is not the confinement.
- It does not connect to another Forgejo, and it does not host more than one.
- It does not let you open or update pull requests from the terminal. The agents do
  that in Forgejo; you review there in a browser.
- It has no scheduler beyond "keep running until stopped", no separate web page or API
  beyond the bundled Forgejo, no cloud service, and no user accounts other than your own
  and the agents'.
- It does not expose the Forgejo to your network. It listens on this computer only.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
