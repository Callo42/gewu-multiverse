# The board

The board is the whole product on one screen: one Forgejo at the top, the agents wired
to it, and a handful of keys. It reads one `BoardView` and sends `Command`s. It never
touches a file, a process or the network; the engine does that. The contract is
`src/engine_api.rs`, and the drawing lives in `src/board/`.

The design rules are short:

- **The picture carries the meaning.** A box is the Forgejo, a continuous line is a
  connection, a card is an agent. Words label; they do not explain.
- **Every state is a symbol and a word.** Colour is emphasis, never the only carrier.
- **No paragraph.** A status is a word or a short phrase; an error is one line naming
  what to do next.
- **Nothing is painted on the terminal's default background.** Every cell has the
  product's own background.
- **One layout rule at every size and count.** The cards sit on a fixed grid: the
  columns are chosen before any row is drawn, so a partial last row keeps the same
  columns and the spine always runs in a gap, never through a card. A card title is
  never truncated and a lone card has no connector stub. When the cards cannot keep
  their titles and all five facts, the board uses one deliberate compact form: one
  wired box with a line (or two columns at the floor) per agent. It still reads as the
  picture — box, wire, agents — and keeps every agent's name, login, state mark, run
  count and last-work word.
- **The Forgejo box keeps its login and repository count at every size.** The version
  is the first fact it gives up; the address, repository count and login stay.
- **A wide window gets a capped picture.** Up to 160 columns the drawing uses the
  width (the repository line and the card detail grow); past that the same picture is
  centred in 160 columns instead of stretching across the screen.

## Screens

The board has eight screens. The first four are the first run; the rest are the product
in use.

| Screen | Opened by | What it is |
|---|---|---|
| Terms | first start, before the terms are recorded | the five fixed warnings, the this-computer confinement line, plus one when runs are not confined |
| Model | after the terms, or `m` from the board | the one model address, name and key |
| Picker | after the model on a first run without agents | how many agents to create, 1–12 |
| Build | `⏎` on the picker | the picture filling in while Forgejo and the accounts are made |
| Home | a finished setup | the Forgejo box and every agent card |
| Agent | `⏎` on a card | one agent's state, instruction and run files |
| Forgejo | `f` or `⏎` on the Forgejo box | its address, version, login, password and agents |
| Help | `?` | every key, grouped by screen |
| Upgrade | first open by a newer build | what this installation was, what was found, what is reused, and one line from this release's notes |

There is no page map of the old text screens and no screen per configuration file. The
settings file and the credentials file are written by the flow and by the engine; a
person does not open them from the board.

## Colour and symbols

| Meaning | Colour | Also |
|---|---|---|
| Forgejo accent, boxes, the model form | orange `#FB923C` | the word `FORGEJO`, the label |
| working, running | green `#4ADE80` | `●` and `working` / `running` |
| next run, downloading, setting up, starting | amber `#FBBF24` | `◐` and the word |
| needs an instruction, needs a model, waiting, stopped with a reason | red `#F87171` | `●` and the words |
| stopped, not set up yet | grey `#94A3B8` | `○` and the word |
| selected card or box | cyan `#22D3EE` | a brighter border and a tinted card |
| wire | slate `#5B6B81` | box-drawing characters (3.86:1 on black) |
| outer frame | slate `#5B6B81` | a complete rectangle (3.86:1 on black) |

The background is `#000000`, body text `#E8E8E8`, secondary text `#94A3B8`, cards
`#0F172A`, a selected card `#083344`.

## One picture, one connection

The Forgejo box is centred at the top. A trunk leaves the centre of its bottom border.
A bus runs under it; the bus has `╭` and `╮` ends, `┬` drops, and a `┴` (or `┼`) joint
where the trunk meets it. Each drop meets the top border of its card as `┴`. When the
cards wrap, a vertical spine leaves the bus at a gap between cards and feeds the next
row's bus; that row's bus joins the spine with `├`, `┤`, `┼` or `┴`, and the spine
continues as `│` through the gap. A last row with fewer cards gets a bus from the
spine to its cards. There is no loose end.

```
             ╭╴ FORGEJO ╶──────────── ● running ╶╮
             │ http://127.0.0.1:3000 · 3 agents │
             ╰────────────────┬─────────────────╯
                              │
          ╭───────────────────┼───────────────────╮
          │                   │                   │
   ╭╴ ● scout ╶──────┴──╮ ╭╴ ○ writer ╶─────┴──╮ ╭╴ ● fixer ╶──────┴──╮
   │ gewu-scout         │ │ gewu-writer        │ │ gewu-fixer         │
   ╰╴ working ╶─────────╯ ╰╴ stopped ╶─────────╯ ╰╴ commented on #12 ╶─╯
```

Titles and statuses sit on a border between two half-line caps, `╴` and `╶`. The caps
close the line before and after the text, so every arm of every box-drawing cell meets
a neighbour's opposite arm. A test renders every screen and every state and checks that
guard on every cell; a loose arm fails the test. The picker preview and the build
picture are checked too.

A row of cards keeps a one-cell margin from the frame on both sides, whether it holds
one card or twenty. The first card never touches the frame's left border.

## First run

The first run is one picture filling in: terms, model, agents, then the build.

### 1. Before you start

The engine supplies the terms, one short plain sentence each. The product shows
exactly these five as one compact numbered block (at most one blank row between
items), a status line under the second item, and a sixth warning — `Runs are not
confined on this computer.` — only when the host offers no automatic confinement or
the layout cannot be confined. The status line states what is true on *this* computer,
either positively — `runs on this computer are confined (Landlock)` (or
`bwrap`/`unshare`) — or as the sixth warning's negative sentence. The footer states the
count (`5 warnings` or `6 warnings`) and the keys. The second item stays conditional
because it describes every mechanism:

```
Agents use this computer with your permissions.
When runs are confined, they cannot read the product's keys, your home or another agent's files, and they start with a cleared environment.
Each has its own login on your Forgejo.
Started agents keep going until you stop them.
Their work is sent to your model provider.
```

The title is `Before you start`. `y` records the terms only after every item has been
drawn at least once; until then it says `read every item, then y`. The status line
scrolls with the second item, so a short window never loses it silently. `n` leaves. `↑`/`↓`
scroll if a window is too short for every item. If the engine found an older
`config.toml` at open, the first screen carries one plain line under the keys: `an
earlier setup was found and left as it was`. That file is never read, converted or
changed.

### 2. Your model

```
╭╴ Your model ╶───────────────────────────────────────╮
│                                                      │
│  API address                                         │
│  https://model.example/v1/chat/completions           │
│  for example https://model.example/v1/chat/completions│
│                                                      │
│  model                                               │
│  demo-model                                          │
│                                                      │
│  API key                                             │
│  ••••••••                                            │
│                                                      │
│  t test   ✓ the model answered                       │
│                                                      │
│  e edit   ↑↓ field   t test   ⏎ next   esc back      │
╰──────────────────────────────────────────────────────╯
```

Three fields, one selected. `e` edits the selected field; `↑`/`↓` (or `tab`) move
between fields. `⏎` commits the field and selects the next one **without** entering its
edit mode, so `t` still tests and the next keystroke cannot overwrite the next value.
`Ctrl-U` (shown as `ctrl-u clear`) empties the selected field. The saved and the edited
key are both eight bullets; an empty key field is empty, with no stray dash. A test
result disappears on the first edit. Continuing with `⏎` runs the same address/key
check as `t`: a wrong address stays on the form with its `✗` line, and a valid but
untested form continues with `not tested — t to test` on the next screen. `t` saves
the model and tests it: `✓ the model answered`, or `✗` with one short reason. The address suffix and the empty key are checked **before any request**:
`✗ wrong address — it should end in /chat/completions`, `✗ the API key is empty`.
Every other line is the provider's refusal and names its status and its own message
(`✗ the provider answered HTTP 404: Add /v1 to the base URL`,
`✗ the provider refused the API key (HTTP 401)`,
`✗ no model called <name> at this address (HTTP 404)`, `✗ no answer within 15 s`). A
404 on a `/chat/completions` path whose address has no `/v1` says the repair first —
`✗ add /v1 — the provider answered HTTP 404: …` — so the correction is on screen at
80×24 even when the provider's message is long. The provider's message is kept whole
(bounded at 400 characters) and the form wraps it to the rows it has; at 80×24 that is
the provider's whole answer, not its first clause.
`⏎` when no field is being edited saves and goes on. The verb is always `⏎ next`,
whether the screen was opened on the first run or by `m`. An empty key with no stored
key is its own error — `✗ the API key is empty` — and sends nothing; a provider that
never saw a request is never said to have refused it. The same notice line never says
the model is saved when its own validation failed. `esc` leaves.

### 3. How many agents?

```
╭╴ How many agents? ╶─────────────────────────────────╮
│  ╭╴ FORGEJO ╶────────────────── ○ not set up yet ╶╮ │
│  │ — · 3 agents                                  │ │
│  ╰───────────────────────┬────────────────────────╯ │
│  −  3  +                 │                           │
│        ╭──────────┬──────┴─────┬──────────╮          │
│        ╰──────────╯            ╰──────────╯          │
│                                                      │
│  ⏎ create   ←→ count   esc back                      │
╰──────────────────────────────────────────────────────╯
```

The count is 3 by default and stays between 1 and 12. `+`/`-` or `←`/`→` change it;
that many empty cards are drawn under the Forgejo box, wired to it with the same bus
and spine. The box says `◐ preview`, never `● running`: no server and no agent exists
until `⏎` sends `CreateAgents` and `StartForgejo` and shows the build. `esc` returns
to the model.

### 4. Setting up

The title is `Setting up…`. The Forgejo box says `◐ downloading 42%`, then
`◐ setting up`, then `◐ starting`, then `● running`. While Forgejo is not set up it
shows `not set up yet`, never an address. A card's login line is `connecting…` until
its account is ready, then it is the login; its status is `waiting for Forgejo` while
waiting and `stopped` once ready. The build ends when Forgejo runs and every login is
ready; a started agent's first run still waits until Forgejo's provisioning has returned
the organisation's repositories, and its card says `waiting for Forgejo` until then. If
Forgejo stops with a reason, the box says `● stopped: <reason>` and `r` retries.

## The board

The Forgejo box is centred at the top: its state and version on the top border, its
address, `team · N repositories` (or `1 repository`) and `N agents`, its newest
repository event on the board's footer row, and `your login: <login>` on its bottom
border. At 72×18 it is three rows: the state and version on top, the address, the
repository count and the agent count in the middle, and the login on the bottom. At
80×24 it grows by one row for the organisation, the whole names that fit and `(+N)` for
the rest (never a half name; room for the tag is reserved before a name is dropped),
and the login; the version is already on the top border. From 100×30 the box is six
rows: an accent bar, the address, the organisation with the whole names, the agent
count, and the login. The version is the first fact given
up when the border is too tight. The box keeps the login and repository count at every
size, is only as tall as its facts, and no row inside it is left empty. The newest
repository event is stored with the settings, so it survives a reopen; when the box
cannot hold it, the board's footer gives it a row of its own, never sharing the keys.

At 72×18 the board is one vertical list of two-row cards while up to three fit: the
top border carries the state symbol, name and state; the bottom border carries the
login, `run N` and the last-work (or newest-failure) word. When four or more would need
a scroll, the same place becomes the compact list: one wired box with a line per
agent, in one column while the line keeps the mark, name, login, run count and word,
otherwise in two columns. The box's bottom border names the three state marks. Every
agent of the twelve the picker offers is then on the screen at once. A home past twelve
(the settings' twenty) keeps two columns and scrolls with the selection.

At 80×24 the cards are three to five rows in up to three columns; the login row also
carries the total `run N` on the right. A three-row card keeps the state word beside
the last-work word on the bottom border, in a tight label with no side padding, so even
`✗ HTTP 500 · stopped` fits; a four- or five-row card gives the state its own body row
and puts only the last-work (or failure) word on the border. From 100×30
a board card is taller: state symbol and name, the login and `run N`, the state word,
the newest activity (`last: … · 2m ago` after a run ends) and the last-work or
newest-failure word. From 140 columns the picture grows with the window: the Forgejo
box widens (so every repository name fits) and cards widen; above 160 the whole picture
is centred in 160 columns, as above.

The one word for the newest work keeps its number: `pull request #100` is `#100`, never
`#10`, and `#10000` stays whole. When the newest run failed, the word starts with the
HTTP status (`✗ HTTP 500 from the model`, or `✗ 500` on a one-line list line) and is cut
on whole words, so the status is never the part that disappears. On a single-column
board the wire junction is placed after the label; a connector is never drawn through
the word.


**The rich details are on the Agent screen, not on the board card.** The board keeps
the few words — name, login, state, one last line, run count. `run N` is the total from
the run records, not a session counter: it does not reset when an agent starts again
or the program reopens. `⏎` on a card opens the Agent screen, which carries the last
work block (`work` + up to three typed rows: `issue #4`,
`pull request #5 from fix/x`, `comment on #1`, or `branch fix/x` when the run pushed
without opening a request or commenting), a tool in flight (`shell · 12s`), a
failed tool call (`⚠ shell: …`, or `✗` when the run itself failed), the newest run's
reason and the instruction. A tool's failure row is the tool's own line (the run's
key is already redacted), so a file name in a refusal is not translated into a
different name. The
work block is kept until a later run does new work; a read-only check run does not
erase it. Every cleanly finished run carries its own final summary as a `result: …` line
beside the mark — `✓` included. `✓` means every status the shell itself reported was 0: each record's `$?` and every
element of every `PIPESTATUS`, not only the statuses the reader managed to pair with a
command. The shell reports every simple command it completes to the product — under
`bash`, through a `DEBUG` trap installed with `set -T` and inherited by subshells,
command substitutions and functions (with an inherited no-op `ERR` trap so a failing
last command there reports itself too). The recorder is installed by a product-owned
`BASH_ENV` prelude, so a nested non-interactive `bash` that reads `BASH_ENV` —
`bash -c …`, `bash script.sh`, a `#!/bin/bash` script — installs the same recorder
into the same file; a nested shell
marks its own frame with `NEST`/`NESTSTART`/`NESTEND`, and its records are paired
inside that frame. A one-command record also carries `PIPESTATUS[0]`, so a condition's
own failure (`if rm …`, `while rm …`, `! rm …`) is not lost to the enclosing
compound's 0. A pipeline record's `PIPESTATUS` is paired inside that shell's frame, fills an
unpaired command, and never replaces a non-zero status an earlier record paired; a
duplicate record from the inherited `ERR` trap is collapsed to one segment. The
records are written to a private file the model's stdout/stderr never carry and
recorded in the transcript as `segments` (`command`, `status`). If any raw status is
non-zero, the newest mark is `⚠` (legend `⚠ cmd`) with the failed command's cause as
its row (`⚠ shell: …`) — never `✓`; it is `⊘` when the failed command is a recognised
write or its output carries refusal evidence. When the pairing cannot name the failed
command, the run stays `⚠` with the raw status's output line as the cause, never `✓`.
A write whose enclosing compound hides its status — `if rm …; then …; fi | cat` — is
`⊘` when the operating system's own diagnostic is in the output, and any program's
diagnostic is a witness, not just a list of programs: a line in the POSIX form
`<program-or-shell>: [line N: ][<path>: ]<strerror>` ending in any error message the
C library itself prints — `Permission denied`, `Read-only file system`, `Too many
levels of symbolic links`, `File name too long` and every other errno text — or
`cannot create`/`cannot remove`/`cannot open`/`cannot stat`/`cannot access` naming a
path, or a line in the shell's own diagnostic prefix, lowers the mark to `⊘` when the
command names a recognised write and to `⚠` otherwise. When a mixed failure prints
more than one diagnostic, the failed write's own line is the cause row.
The shell's own voice is the third witness. The product launches the shell, so it
knows the exact `<name>: ` with which bash prefixes its own errors and warnings: the
outer shell's name and the `$0` of every nested `bash` that reads `BASH_ENV` arrive
through the recorder's `SHELLNAME` record, the trap reports `BASH_SOURCE[0]` (the
name bash uses for a sourced command or a function defined there), and a function
defined on the `-c` line is named `environment`.
Any output line in that form — `<name>: line N: <message>` or `<name>: <message>` —
is shell-reported trouble whatever the text says, so no message list is kept:
`ambiguous redirect`, `cannot overwrite existing file` and every other shell wording
are seen. With an all-zero raw record it makes the run `⚠`, or `⊘` when the command
names a recognised write or when the shell's record names a write whose plain target
the diagnostic carries (a noclobber overwrite inside `bash script.sh`); it can only
lower the mark.
A failure that leaves neither a status nor any diagnostic text the system or the
shell prints cannot be seen: limits (f) and (g) below. A recognised shell write is a
`DELETE`/`PUT`/`POST`/`PATCH` in any standard curl spelling, a non-GET `--data`, a git
write read by subcommand position (`push`, `rm`, `mv`, `add`, `commit`, `stash`,
`pull`, `fetch`, `branch`/`tag` changes, and the commands that move HEAD or the
working tree; `git show push` is a read), `rm`, `rmdir`, `unlink`, `mv`, `cp`, `tee`,
`truncate`, `sed -i`, or a `>` redirection into a file. A write whose command exits
non-zero is a refusal whatever its output says; a failed command that is not a write
is `⚠`. On a host without `bash` the shell is `/bin/sh` and the product falls back to
reading the output: a compound command is split on `;`, `&&`, `||`, `|` and newlines
outside quotes, and a write segment plus a failure-context line (`fatal:`, `error:`,
`remote rejected`, `failed to push`, `Permission denied`, `denied`, an HTTP 4xx/5xx
status line, `curl: (22)`) is a refusal even when the wrapper exits 0; the run log
says the recorder is off. A `curl` without `-f` exits 0 on a 4xx/5xx, so that one
call's output is always read for a status line (`HTTP/1.1 404`, a whole-line `-w`
code) or an HTTP error phrase (`Forbidden`, `Unauthorized`, `permission denied`); a
write that succeeded is not otherwise re-read for words — a success body that says
`not found` or `404`, and a file named `refused`, are not refusals. Seven limits are
stated as built: **(a)** a model that forges or truncates the record file, closes the
recorder (unsets or overrides `BASH_ENV`), masks a failure in a tool that prints
nothing, or suppresses both the status and the output of a write hidden inside a
compound, defeats the mark (the transcript and `l` keep what the tool printed,
which may be nothing); **(b)** a `curl`
without `-f` whose 200 body literally says `Forbidden` is called refused — the safe
direction, and contrived; **(c)** the rule is deliberately conservative and can only
lower the mark, so a condition that failed and was recovered from (`if false; …`), a
system error string from a command that did not fail, or a line that merely happens
to start with a shell name may read `⚠` — never the other way; **(d)** on a host
without `bash` the shell is `/bin/sh` and
the product falls back to the output heuristics above (the run log says the recorder
is off); **(e)** a nested interpreter that does not load the product's recorder — any
non-bash interpreter (`sh -c` when `/bin/sh` is not bash, `python -c`, `perl -e`), or
`bash` in POSIX or privileged mode (`--posix`, `-p`, `POSIXLY_CORRECT=1`) — runs
outside the record, so its failed write is seen only through the outer command's exit
status and the output heuristics;
**(f)** a failure that leaves neither a raw status nor any diagnostic text — a
program that fails silently inside a piped condition — cannot be seen; and **(g)** a
failure inside a piped or negated condition reported only by a program's own
free-form message (neither a system error string nor a line in the shell's own
prefix), with an all-zero raw record, cannot be seen. In (f) and (g) the transcript
and `l` keep what there was.
A destructive instruction answered
only in prose — `I left it in place`, `I did not delete it`, `deleting it is not safe`
— also gets `⊘`, so a refusal without a tool call is not mistaken for success; a keep
phrase beside a success verb for the destructive act (`I deleted the stale branch and
kept the docs as is`) stays `✓`. When the newest run's log says no issue was assigned to the agent, the work area
says `no issues assigned to gewu-agent-1 yet`. The instruction's first display row is always drawn; its section has a rule row of its
own, and the text never shares a row with that rule. `… (N more lines)` counts the
**file lines** of the document that did not fit, not the wrapped display rows, and the
noun agrees (`1 more line`). At the 72×18 floor the section keeps the first display
row; where a second row exists the marker goes on the next row. From 80 columns the
taller panel uses its spare rows: more display rows, and at a tall size the whole
instruction. The run strip is bounded: at most five marks, then `+N`, with a small
legend, the newest mark's one-word label, and the run count on the right. The selected card has the cyan border and tinted background.

| Agent state | Status |
|---|---|
| running, with activity | `working` and `commented on #12` / `opened pull request #4` |
| running, no activity | `working` |
| between runs | `next run in 8s` |
| needs an instruction | `needs an instruction` |
| needs a model | `needs a model` |
| Forgejo not running, or its repositories not back yet | `waiting for Forgejo` |
| stopped | `stopped` |
| login not ready | `waiting for Forgejo` |
| login failed | `login failed` |

Any number of agents from 1 to 20 is drawn. Cards wrap into rows; each row has its own
bus and the spine reaches every row. When the rows do not fit, the view scrolls with the
selection and the footer says `1–8 / 20 ↓`.

The keys are on two lines:

```
←→ pick   ⏎ open   s start/stop   + add   − remove
f Forgejo   m model   ? help   q quit
```

When this computer cannot confine an agent's shell, the line under the keys says
`runs are not confined on this computer`; the same sentence is the sixth warning item
on the first-run screen. The selected card carries `▸` before its name on the border
(and the selected list line before its mark), so a colour-blind or monochrome reader
can see which agent `←→` targets; the tint is only reinforcement. The newest event,
when the box cannot hold it, is on its own footer row under the keys. `BoardView.notice`
is drawn once on the status line under the keys. It is cleaned before it
is drawn: the engine writes names in backticks, the board removes them, so a notice
reads `agent-4 is connected as gewu-agent-4` and never shows the marks.

`↑` from the first row selects the Forgejo box; `⏎` on it opens the Forgejo screen.
`f` opens it from anywhere on the board. `+` makes a **new** `agent-N` — the number only moves forward, so a removed
`agent-2` is never handed out again and its deactivated account and history stay
untouched (at most 20 numbers); `−` asks once, then removes the selected agent. The
last agent cannot be removed and the screen says so.

## An agent

```
╭╴ agent-1 ╶──────────────────────────────────────────╮
│        ╭──────────────────────────────────────╮      │
│        │ ● working                 gewu-agent-1│      │
│        │ commented on #12                     │      │
│        │                                      │      │
│        │ instruction                          │      │
│        │ fix the flaky login test             │      │
│        │                                      │      │
│        │ last runs  ✓ ⊘ ∅ ✗          run 7    │      │
│        ╰──────────────────────────────────────╯      │
│  s stop   e instruction   l log   t transcript       │
│  j raw JSONL   d remove   esc back                   │
╰──────────────────────────────────────────────────────╯
```

The big card carries state, login, the last work block, the instruction's first
lines (or `no instruction — e writes one`) and a strip of the newest run ends: `✓`
succeeded, `⊘` **write** refused, `∅` no work, `⚠` finished with a failed command,
`✗` failed, `◷` timed out, `■` stopped. The label is `last runs`, at most five marks
then `+N`, the newest mark's one-word label (`⊘ refused`, `∅ no work`, `⚠ cmd failed`),
the same legend at every size (`✓ ok ⊘ refused ∅ no work ✗ failed ⚠ cmd ■ stopped ◷
timed out`; at the 72-column floor it wraps whole on two rows), and the total count
`run 7`. A refused read is never `⊘`: a
failed read inside a clean run is `⚠`. The summary line prefers the newest run that did
work (`last work: …`); a later no-op's own reason is labelled `no work: …`, so it is
not presented as the agent's work.

`s` starts or stops; `e` hands `instruction_path` to `$EDITOR`; `l` hands the newest
run's log to the pager; `t` writes and shows a readable transcript (turns, tools and
their output) and `j` shows the raw JSONL record. `d` draws the
consequence and the keys before it acts: `remove agent-1? login revoked, account
deactivated, history kept` on one line and `y remove   n keep` under it. The account row and its history stay;
the Forgejo account is deactivated, so a web sign-in is refused too. `d` on the last
agent says `the last agent cannot be removed` instead of doing nothing. `esc` returns to
the board. A hand-off steps out of the terminal and restores it exactly.

## Forgejo

```
╭╴ Forgejo ╶──────────────────────────────────────────╮
│                                                      │
│  address        http://127.0.0.1:3000                │
│  version        17.0.0                               │
│  organisation   team                                 │
│  repositories   3                                    │
│  your login:    owner                                │
│  password       ••••••••••  p resets it — 10 s, old…│
│  agents can     write to team repositories only      │
│  agents work    only on issues assigned to them — … │
│  team page      http://127.0.0.1:3000/org/team/…    │
│  agents         gewu-agent-1 · gewu-agent-2          │
│                                                      │
│  open the address in a browser to see their work.    │
│                                                      │
│  p password   esc back                               │
╰──────────────────────────────────────────────────────╯
```

The screen uses the state words above. While Forgejo is not installed, `address` says
`not set up yet`, not an address. `repositories` lists the organisation's repositories
by name; `agents can` is the one-line permission summary; `agents work only on issues
assigned to them — assign an issue in Forgejo` is the work rule (the list tool shows
only its own assigned issues; a write outside that scope is refused); `team page` is
`/org/<organisation>/teams/<team>` (never `/orgs/`, which is a 404). At 72×18 the page
keeps every row, wrapping the two long ones instead of dropping version, organisation
or the team page. A login kept from an installation made by an earlier
version says `(kept from an earlier setup)`. Repository changes seen between two
administration-API polls appear on the page and on the board at every supported size:
`team/scratch was deleted`, `team/website was created` (inside the box where it is tall
enough, otherwise on the board's footer line).

`p` sends `RevealAdminPassword`; the engine resets the password through Forgejo's
administration API (the value travels in the request body, never in a process
argument) and the new value is shown in the next view only, for ten seconds. It wraps
whole at 80 columns — deliberately two lines at that width, never cut with `…` — and
the static row says `p resets it, c copies — shown for 10 s, old stops`: the password
shown before that moment no longer works. `c` puts the shown value on the terminal's
clipboard with one OSC 52 sequence (base64), never a file, a log or a second plaintext
line, and shows `copied`; a terminal without OSC 52 ignores the sequence and the screen
still shows the value. The agents' own logins are listed, never their
keys.

## Model and keys

`m` opens `Your model` from the board; `⏎` saves and returns, `t` tests.

`?` opens `Keys`, grouped, one line each:

```
Keys
board    ←→ pick   ⏎ open   s start/stop
         + add     − remove   f Forgejo
         m model   ? help   q quit
agent    s start/stop   e instruction   l log
         t transcript   j raw   d remove   esc back
model    e edit   ↑↓ field   t test   ⏎ next
         ctrl-u clear   esc back
Forgejo  p password   esc back
first run  y agree   n leave
```

`q` stops everything. If any agent is started, the footer first says exactly:

```
quit stops 3 agents and Forgejo; they start again on reopen — q again quits
```

The noun agrees (`1 agent`, `2 agents`), and the warning names only agents with a live
run (running or waiting for the next run):
`quit stops agent-2 and agent-3, and Forgejo; they start again on reopen — q again
quits`. An agent that is started but has no instruction is not named.

The warning uses every footer row the geometry gives (three at 72 and 80 columns, four
when a notice or the status line needs one): it wraps on whole words, is never
abbreviated, and the consequence, the reopen sentence and `q again quits` are always
kept. If a home past twelve has more running agents than the rows can name, the count
keeps what stops rather than dropping the consequence. A second `q` sends `shutdown`.
With every agent stopped, `q` quits at once and draws no warning.

## Key map

| Screen | Key | Effect |
|---|---|---|
| Before you start | `y` | `AcknowledgeTerms` once every item has been drawn |
| | `n` | leave |
| | `↑` `↓` | scroll |
| Your model | `↑` `↓` `tab` | move between address, model, key |
| | `e` | edit the selected field |
| | `⏎` (editing) | keep and move to the next field; on the key, finish editing |
| | `⏎` (not editing) | `SetEndpoint`, then next/save |
| | `t` | `SetEndpoint`, then `TestEndpoint` |
| | `esc` (editing) | stop editing, stay on the form |
| | `esc` (not editing) | back |
| How many agents? | `←` `→` `+` `-` | choose 1–12 |
| | `⏎` | `CreateAgents`, `StartForgejo` |
| | `esc` | back to the model |
| Setting up | `r` | retry a failed `StartForgejo` |
| | `q` | quit |
| board | `←→↑↓` | select an agent; `↑` from the first row selects Forgejo |
| | `⏎` | open the selected agent or Forgejo |
| | `s` | `Start` or `Stop` the selected agent |
| | `+` | `AddAgent` (at most 20) |
| | `−` | ask once, then `RemoveAgent`; the last agent says why it cannot |
| | `f` `m` `?` | Forgejo, model, help |
| | `q` | quit; asks once when an agent is started |
| agent | `s` | `Start` or `Stop` |
| | `e` `l` `t` `j` | editor, log, readable transcript, raw JSONL hand-off |
| | `d` | ask once, then `RemoveAgent` |
| | `esc` | back |
| Forgejo | `p` | `RevealAdminPassword` |
| | `c` | copy the shown password (OSC 52) |
| | `esc` | back |
| Keys | `esc` `?` | close |

## Words

The engine states are mapped to the words above in one place, `src/board/status.rs`.
No screen uses the words `session`, `provisioning`, `token`, `endpoint`, `missing`,
`blocked`, `process group`, `loopback` or `2xx`; a render test scans every frame for
them. A notice is cleaned through the same map and loses its backticks.

The same test scans every frame at every supported size for this vocabulary, and
another checks that every card and every wire is whole and connected.
