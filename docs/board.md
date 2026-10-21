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
- **The smallest window is 72×18.** At 72×18 the cards are one vertical list with one
  spine; from 100×30 they grow and the Forgejo box shows its facts.
- **A wide window gets a capped picture.** Past about 140 columns the same drawing is
  centred in 100 columns instead of stretching across the screen.

## Screens

The board has eight screens. The first four are the first run; the rest are the product
in use.

| Screen | Opened by | What it is |
|---|---|---|
| Terms | first start, before the terms are recorded | the five fixed warnings, plus one when runs are not confined |
| Model | after the terms, or `m` from the board | the one model address, name and key |
| Picker | after the model on a first run without agents | how many agents to create, 1–12 |
| Build | `⏎` on the picker | the picture filling in while Forgejo and the accounts are made |
| Home | a finished setup | the Forgejo box and every agent card |
| Agent | `⏎` on a card | one agent's state, instruction and run files |
| Forgejo | `f` or `⏎` on the Forgejo box | its address, version, login, password and agents |
| Help | `?` | every key, grouped by screen |
| Upgrade | first open by a newer build | what this installation was, what was found, and what is reused |

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
| wire | slate `#475569` | box-drawing characters |
| outer frame | slate `#475569` | a complete rectangle |

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

The engine supplies the terms, one short item each. The product shows exactly these
five, and a sixth — `Runs are not confined on this computer.` — only when the host
offers no automatic confinement (Landlock) or the layout cannot be confined. The
second item names the mechanism: Landlock is automatic; `bwrap` and `unshare` are
opt-in with `GEWU_CONFINEMENT` and are tested, not yet proven on a real host:

```
Agents use this computer with your permissions.
Where runs are confined, they cannot read the product's keys, your home or another agent's files; they start with a cleared environment and still see /etc, /proc, /sys, /run, the installed tools and the network. The automatic confinement is Landlock; bwrap and unshare need GEWU_CONFINEMENT and are tested, not yet proven on a real host.
Each has its own login on your Forgejo.
Started agents keep going until you stop them.
Their work is sent to your model provider.
```

The title is `Before you start`. `y` records the terms only after every item has been
drawn at least once; until then it says `read every item, then y`. `n` leaves. `↑`/`↓`
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
│  e edit   t test   ⏎ next   esc back                 │
╰──────────────────────────────────────────────────────╯
```

Three fields, one selected. `e` edits the selected field; `⏎` keeps it. `↑`/`↓` or
`tab` move. `t` saves the model and tests it: `✓ the model answered`, or `✗` with one
short reason (`✗ wrong address — it should end in /chat/completions`, `✗ the API key was
refused`, `✗ no model called <name> at this address`, `✗ no answer within 15 s`).
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
and spine. `⏎` sends `CreateAgents` and `StartForgejo` and shows the build. `esc`
returns to the model.

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

The Forgejo box is centred at the top: a header bar, its state, its address,
`team · N repositories` (or `1 repository`), `N agents`, the version and
`your login: <login>`. At 72×18 it is three rows and shows the state, the address (or
`—`), and the agent count. From 100×30 it is at most eight rows: the accent bar, the
address, the team and count, the agent count, the version and the login. No row inside
it is left empty.

At 72×18 the board is one vertical list: one card per row under the Forgejo box, wired
by one spine, with the same three-line card (state symbol and name on the top border,
the login and `run N` in the middle, one status on the bottom border). Cards that do
not fit scroll with the selection.

At 80×24 the cards are three rows in up to three columns; the login row also carries
`run N` on the right, and the bottom border carries the status. From 100×30 a board
card is taller: state symbol and name and `run N` on the first row, the login, the
status, and the newest activity (`last: … · 2m ago` after a run ends).

**The rich details are on the Agent screen, not on the board card.** The board keeps
the few words — name, login, state, one last line, run count. `⏎` on a card opens the
Agent screen, which carries the last work block (issue, pull request, branch, comment),
a tool in flight (`shell · 12s`), a failed tool call (`✗ shell: …`), the newest failed
run's first reason, and the instruction. The instruction is the first line or
`… (N more lines)`; its section has a rule row of its own, the text never shares a row
with that rule, and a long one-line instruction is signposted too: the wrapped display
rows are counted and `… (N more lines)` names what did not fit. At the 72×18 floor
the section has a single content row, so it draws the marker alone (`… (N more
lines)`); where a second row exists the first display row is kept and the marker goes
on the next row. The run strip is
bounded: at most five marks, then `+N`, with a small legend, and the run count on the
right. The selected card has the cyan border and tinted background.

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
on the first-run screen. `BoardView.notice` is drawn once on the line under the keys. It is cleaned before it
is drawn: the engine writes names in backticks, the board removes them, so a notice
reads `agent-4 is connected as gewu-agent-4` and never shows the marks.

`↑` from the first row selects the Forgejo box; `⏎` on it opens the Forgejo screen.
`f` opens it from anywhere on the board. `+` makes the next free `agent-N` (at most 20);
`−` asks once, then removes the selected agent. The last agent cannot be removed.

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
│        │ last runs  ✓ ✓ ✗            run 4    │      │
│        ╰──────────────────────────────────────╯      │
│  s stop   e instruction   l log   t transcript       │
│  d remove   esc back                                 │
╰──────────────────────────────────────────────────────╯
```

The big card carries state, login, the last work block, the instruction's first
lines (or `no instruction — e writes one`) and a strip of the newest run ends: `✓`
succeeded, `✗` failed, `◷` timed out, `■` stopped. The label is `last runs`, at most
five marks then `+N`, a one-line legend, and the count `run 4`.

`s` starts or stops; `e` hands `instruction_path` to `$EDITOR`; `l` and `t` hand the
newest run's log or transcript from `session_files` to the pager. `d` draws the
consequence in one sentence before it acts: `remove agent-1? token revoked, account
deactivated, history and folder kept — y/n`. The account row and its history stay;
the Forgejo account is deactivated, so a web sign-in is refused too. `esc` returns to
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
│  password       ••••••••••   p resets it — old stops │
│  agents can     write to team repositories only      │
│  team page      http://127.0.0.1:3000/orgs/team/…    │
│  agents         gewu-agent-1 · gewu-agent-2          │
│                                                      │
│  open the address in a browser to see their work.    │
│                                                      │
│  p password   esc back                               │
╰──────────────────────────────────────────────────────╯
```

The screen uses the state words above. While Forgejo is not installed, `address` says
`not set up yet`, not an address. `repositories` lists the organisation's repositories
by name; `agents can` is the one-line permission summary; `team page` is the address of
the team the agents belong to. A login kept from an installation made by an earlier
version says `(kept from an earlier setup)`. Repository changes seen between two
administration-API polls appear on the page and on the board: `team/scratch was
deleted`, `team/website was created`.

`p` sends `RevealAdminPassword`; the engine resets the password through Forgejo's
administration API (the value travels in the request body, never in a process
argument) and the new value is shown in the next view only, for about ten seconds. It
wraps whole at 80 columns and is never cut with `…`. The consequence is a static row,
not a transient notice: `p resets it — the old one stops working`. The agents' own
logins are listed, never their keys.

## Model and keys

`m` opens `Your model` from the board; `⏎` saves and returns, `t` tests.

`?` opens `Keys`, grouped, one line each:

```
Keys
board    ←→ pick   ⏎ open   s start/stop
         + add     − remove   f Forgejo
         m model   ? help   q quit
agent    s start/stop   e instruction   l log
         t transcript   d remove   esc back
model    e edit   t test   ⏎ next   esc back
Forgejo  p password   esc back
first run  y agree   n leave
```

`q` stops everything. If any agent is started, the footer first says exactly:

```
quit stops 3 agents and Forgejo; they start again when you reopen — q again quits
```

The noun agrees: `1 agent`, `2 agents`.

At 72 columns the line wraps; it is never abbreviated. A second `q` sends `shutdown`.
With every agent stopped, `q` quits at once and draws no warning.

## Key map

| Screen | Key | Effect |
|---|---|---|
| Before you start | `y` | `AcknowledgeTerms` once every item has been drawn |
| | `n` | leave |
| | `↑` `↓` | scroll |
| Your model | `↑` `↓` `tab` | move between address, model, key |
| | `e` | edit the selected field |
| | `⏎` (editing) | keep the field |
| | `⏎` (not editing) | `SetEndpoint`, then next/save |
| | `t` | `SetEndpoint`, then `TestEndpoint` |
| | `esc` | back (or leave editing) |
| How many agents? | `←` `→` `+` `-` | choose 1–12 |
| | `⏎` | `CreateAgents`, `StartForgejo` |
| | `esc` | back to the model |
| Setting up | `r` | retry a failed `StartForgejo` |
| | `q` | quit |
| board | `←→↑↓` | select an agent; `↑` from the first row selects Forgejo |
| | `⏎` | open the selected agent or Forgejo |
| | `s` | `Start` or `Stop` the selected agent |
| | `+` | `AddAgent` (at most 20) |
| | `−` | ask once, then `RemoveAgent` |
| | `f` `m` `?` | Forgejo, model, help |
| | `q` | quit; asks once when an agent is started |
| agent | `s` | `Start` or `Stop` |
| | `e` `l` `t` | editor, log, transcript hand-off |
| | `d` | ask once, then `RemoveAgent` |
| | `esc` | back |
| Forgejo | `p` | `RevealAdminPassword` |
| | `esc` | back |
| Keys | `esc` `?` | close |

## Words

The engine states are mapped to the words above in one place, `src/board/status.rs`.
No screen uses the words `session`, `provisioning`, `token`, `endpoint`, `missing`,
`blocked`, `process group`, `loopback` or `2xx`; a render test scans every frame for
them. A notice is cleaned through the same map and loses its backticks.

The same test scans every frame at every supported size for this vocabulary, and
another checks that every card and every wire is whole and connected.
