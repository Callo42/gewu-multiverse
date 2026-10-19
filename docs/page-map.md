# The page map

Every screen the product can show, and every state a user can be in on it: what the
screen shows, and **what it must offer a zero-knowledge user at that moment**. The map
is the standard the screens are built and fixed against; a screen that hides the next
step, or shows a word the user has not been given, is a defect against its row. This
map governs the tree it is committed in; a release carries the map as of its own tag,
so a reader can tell "the product fails its map" from "the map is ahead of the
product".

A zero-knowledge user has started the product and nothing else. They have not read this
repository, the README or any page. The product carries them: each state says what it
is, what is missing, and the key or the form that moves them on.

**Nothing truncates in silence — in both directions.** A panel or a note area whose
wrapped content does not fit its window says so where the user is looking: a bordered
panel puts `more below — press ]` in its title (always drawn), and the two
borderless note areas (Agents, Runs) reserve their last drawn row for the same
sentence. `]` and `PageDown` move **one viewport** of a panel or note area, and `[`
and `PageUp` move the inverse, on every screen; lists move with `up`/`down`. The
title also carries `line n of m`, the row on show
against every wrapped row. The scroll offset is clamped when a window grows or
shrinks, so a resize can neither leave the window past the end nor hide the row the
person was reading. The declaration names the key that actually moves the surface;
`[` and `]` are the panel keys on every screen, and the same keys return a scrolled
surface toward the top. A bordered panel never draws **half a wrapped line**: whole
logical lines are dropped from the bottom until the rest fits, so a sentence never
ends at the box edge — the `more below — press ]` declaration and `[`/`]` are what
carry the rest. The scroll offset counts **wrapped rows**, and a logical line taller
than the body is drawn row by row rather than dropped, so no line is ever unreachable
however short the body is (W36-1); a row is still drawn whole or not at all. A scrolled
title keeps its declaration whole: the compact form still names the real keys (`[`/`]`),
and the title shortens its own name before it shortens or clips them. While **any overlay owns
the screen** — the first-run acknowledgment, the add-agent form, a secret or address
prompt, a removal or leftover-delete confirmation, the editor confirmation or the key
help — the note area behind it is **not drawn at all** — neither its content nor its
overflow declaration: a bordered overlay could cover the head of a wrapped sentence and
leave its tail (`writes both files for you.`) as the first thing a zero-knowledge user
reads, and the declaration would point at rows the overlay hides (W-5, F-1, S-1, R-3);
the area returns whole the moment the overlay closes. The rule is one predicate over
every overlay, not a list, so a new overlay cannot leave the fragment behind.
A surface scrolled away from its own top carries the sibling marker —
`↑ more above — press [` in a bordered title, `↑ more above — press [` as the first
drawn row of a note area — so a scrolled panel never hides its own heading in silence.
**Every modal and overlay that can overflow is a bordered surface too**: its body
scrolls with the real keys (`[`/`]`) and keeps its answer row (the key that answers it)
pinned at the bottom, so a modal at the 72×18 floor can reveal every dropped sentence
and its answer is never the part that scrolls away (F-1). The title carries
`more below — press ]` / `↑ more above — press [` only while the body really extends
that way; the read-only review scrolls its terms too, and `[`/`]` do not close it. A box
may not declare `more below — press ]` from a body capped at a fixed height: the scroll
keys really do draw the rest (w44 F2). The **windowed** add-agent form is the one
exception: `[`/`]` are ordinary data in its fields (an IPv6 URL or a bracketed paste), so
its title names the keys that actually move between dropped fields — `tab`/`↑`/`↓` — and
its cursor row and `ctrl-s`/`esc` action stay drawn (LIB-1). The declaration is the part that
must survive: when a panel's title carries more than
its name (the Forgejo panel carries its repository range) and both cannot fit, the
screen name is shortened and then the declaration is shortened, and only if neither
can hold both does the range move to a line above the list. A column that shows an
agent's **name** flexes to its content, up to a 32-column cap, so a wide window shows
whole legal names instead of ellipsizing them beside unused width; the model column
does the same. Only when the content genuinely cannot fit is a name shortened with an
ellipsis that keeps both ends, never clipped mid-word with no marker. **The state
column is paid its full class width first** (`failed (server_error)`), and the name
column takes what is left: at the 72-column floor a long name is shortened before the
state word is, because a different failure word is a false fact while a shortened name
still marks its row on a screen where the same agent is selected. On a wider window
both fit, one column of right padding stands before the border, and the spare width is
trailing blank space after `state`. A login and its email are one fact: their
separating space is a no-break space, so a wrap at the line edge can never eat it and
leave `someone-else` glued to `<someone@else.invalid>` (N-2). **The scroll offset belongs
to the screen it was made on**: every way of changing screens (`1`–`6`, tab and
left/right, `esc`, `enter` into an agent) returns the new screen to its own top, so a
screen the user never scrolled never opens at another screen's offset. **The status
sentence belongs to the screen that made it**: the same screen change clears it, so no
sentence about a panel, a key or an action outlives the screen it was made on (C-1);
what must persist across screens is drawn from the state that owns it — the note areas
read the records, the acknowledgment line reads the record — never from the status line.
**A refusal replaces the optimistic sentence it started from**: after a write is
refused, the footer holds the refusal (the same sentence the prompt or the box holds),
not the paste that preceded it (F-2).
**The below-fold
content is reachable too**: `[`/`]` scroll the panel or note area on **every** screen —
Forgejo included, where the list, the summary, the identity report and the key legend
are the one panel, so below 80×24 the same discipline reaches them instead of only
declaring them; on the screens whose content is not a list (Agent, Instruction,
Credentials) the arrows and page keys move the panel, and a press that cannot move it
says `the panel is at its top` / `at its end` rather than being swallowed. A screen's
own must-offer sentence is drawn at the advertised minimum size (80×24) by putting the
offer above the prose that can wait, not by making the window taller; only what is
below that line needs the declaration.

**A paste is a paste, and a field holds only what the person put there.** The terminal
frames a bracketed paste as one event, and the product never replays it as keystrokes.
A one-line field keeps the first line, names the rest as unused **when content was
actually dropped — a trailing newline or whitespace is named as ignored, never as
lost paste content (W32-4)** — and **replaces** the field rather than doubling it; with
no field open the paste is dropped with a sentence naming where it can land (`press n
for the form, or k/f/v/A for a secret`); under an overlay that owns the keys the status
says what owns the screen. Prompts open empty: an old value is a dim placeholder, not
text the cursor sits after, so typing replaces it. `ctrl-u` clears the field it names.
A URL is checked for a scheme and a host before it is written; a refused value stays
in the field and the box carries the reason. The new instruction template is empty,
so no developer sentence can reach the model as if it were the person's instruction. A byte stream the terminal does **not** frame as a paste (a
program injecting raw bytes) is indistinguishable from typing, and the product is not
asked to answer for it (F4).

**One custody for everything the product writes.** Every directory the product creates
under its configuration and state roots is held to mode `0700` — the configuration
root, `agents/` and each agent, skill and workspace directory, and the state root with
each agent, run and run-skills directory. Every file it writes is `0600` — the
configuration file, the credentials file, the acknowledgment record (which also carries a
digest of the terms text, not a signature, C-4), the `last-seen` version record (U3), the
endpoint-test record, the role-check record, the agent's `previous-runs` note, the instruction,
steps and skill templates, the plan, the log and the transcript. The one exception is
the `systemd` unit, which is `0644` because it holds no secret: it names paths only,
and the directory around it is `0700`. The Credentials screen states the credentials
file's real mode; the longer same-custody explanation is in the `?` key guide (F-1,
F-2, F6). The custody is also **restored to a tree an older version left**: on every load the product holds its own configuration and state roots back to `0700` directories and `0600` files (not descending into `workspace`, whose clone is git's), so a run directory or configuration file an older version wrote is repaired instead of kept at its old umask mode (R-1).

**Colour and emphasis are one scheme, not decorations.** The product paints its own
background on every cell — RGB `(0, 0, 0)`, including the full-width band every overlay
clears before it draws its box — and its own foreground, so the same
ratios hold on a dark and a light terminal instead of depending on a theme's default.
The note area, the footer and the tables use the same four meanings: an **error** is
RGB `(255, 95, 95)` plus bold plus a literal `error`/`refused`/`failed` word; a
**warning** is RGB `(255, 215, 0)` plus bold plus its own word; **information** is RGB
`(230, 230, 230)`; **selection** is black on RGB `(0, 205, 205)` plus bold and the `>`
marker. Muted text is RGB `(168, 168, 168)`, not terminal dim, because a renderer is
free to make `DIM` arbitrarily faint. Measured against the explicit black background,
error is 7.1:1, warning is 15.0:1, information is 16.8:1, muted is 8.8:1 and selection
is 10.6:1 — all at least 4.5:1. No meaning is carried by hue alone: the selection keeps
its `>`; the tables keep the state word; an error always keeps its word even if a
colour-blind reader or a monochrome capture loses the hue. A transport failure and a
quit warning are never uncoloured: the footer classifies them as an error and a
warning respectively. There is one overlay treatment: a cyan border, the same painted
background and the same dim title chrome for terms, forms and confirmations; red and
yellow remain the meanings of the words inside a box, not different box styles.

**No screen prints a host name or machine id.** A record may carry the full machine
mark in its private file; a screen turns that mark into `this machine`, `another
machine` or `a machine it cannot name`. The mark is for record semantics, not for a
footer, a role check or a screenshot.

The product's vocabulary is small and fixed: **agent**, **endpoint**, **model**,
**instruction**, **skill**, **steps**, **run**, **role**, **identity**, **credentials
file**, **configuration file**, **supervision**, **concurrency**, **max turns**. A
screen may use a word only after the screen, or a screen before it, has shown what it
means. `concurrency` (how many model requests one agent may keep in flight at once)
and `max turns` (the most model turns one run may take) are defined in the `?` key guide;
the hub keeps one compact status line and sends the definitions there. A transport failure is shown on screen in the product's
own words (`the endpoint refused the connection`, `the connection dropped`, `the
address could not be resolved`, and for a peer that closes after reading the request,
`the request was sent, but the endpoint closed the connection` — the transport's own
`io:` prefix never reaches a screen, S-4); the run's log and transcript keep the transport's
exact sentence, so the failure is still auditable afterwards. A count in a plain-text
line reads as a sentence (`1 turn`, `2 turns`, `0 repositories`), never a `turn(s)`
gutter; the JSONL transcript keeps its numbers.

Screens: **Agents** (`1`) · **Agent** (`2`) · **Instruction** (`3`) · **Forgejo**
(`4`) · **Credentials** (`5`) · **Runs** (`6`); the tab bar carries each number beside
its name, and every name survives whole at the advertised floor (72×18) — the bar
sheds its side padding rather than clipping the last name to a fragment. Overlays:
**first-run
acknowledgment**, **add-agent form**, **key help**. Hand-offs: **editor**, **pager**.

## This round's states

These are states of the map above, fixed in this tree, not a second map.

**Endpoint shape and editing.** An endpoint is the full URL a run POSTs to. The add
form and the Agent screen say so with the example
`https://host/v1/chat/completions`; a base URL ending at `/v1` is not enough. `E` on
the Agent screen opens one bordered editor for the endpoint, the model id and the
model key of the selected agent, without removing it. The three values stay as typed;
`tab`/`↑`/`↓` move, `ctrl-s` writes, `esc` cancels, and a refused URL keeps the editor
open with its reason directly under the URL row at every size. The editor windows its
body so the cursor row and the refusal row are always drawn, and the
`ctrl-s writes it / esc cancels` line is pinned inside the border. An endpoint test
first POSTs the configured
full URL. On HTTP 404 it asks the endpoint's models list (the full URL with
`/chat/completions` replaced by `/models`): a listed model means `route_not_found`
(the path is wrong; the correction names `/v1/chat/completions` in full), a successful
list without the model means `model_not_found`, and a models request that does not
answer keeps the class `route_not_found` because the model was never disproved. Every
failure class — `server_error` included — is drawn with the same
`endpoint <url> — model <id>` phrase, and a run's stored reason carries it too.

**A field holds what was typed.** Secret, address and edit prompts open empty and show
the old value only as a dim placeholder where one exists; `ctrl-u` clears the open
field. A URL (model endpoint or Forgejo address) is checked for `http`/`https` and a
host before a write; a refusal stays in the box with the same reason on the status
line. The first instruction template is empty, so no product sentence is ever sent.

**The floor keeps one action row.** Every screen reserves one body row above the
footer for its own actions (`n`/`enter` on Agents; `E`/`e`/`p`/`s` on Agent;
`e`/`t`/`K` on Instruction; `u`/`c`/`C`/`v`/`A`/`i` on Forgejo; `k`/`f`/`A` on
Credentials; `o`/`T` on Runs), each ending `? all keys`. The add-agent form pins
`ctrl-s writes it / esc cancels` as its last body row at every size; at 72×18 its
compact introduction leaves two or three named fields visible. At 72×18 the Runs list
collapses to one summary line naming `up/down` and the records panel takes the rest of
the body; the summary says which runs are on show and how to page the list. Instruction
puts the preview directly under its action line, draws every preview row the panel can
show, and has no `N more lines` cap. Forgejo draws the identity report once after the
repository list, and when the window can show only part of the list the line says so
truthfully (`repositories 1–1 of 4 — ↓ more`). The terms and read-only review are
sized to their content where the window can hold it; their answer line is the last
thing after the content, and only the answer itself is pinned.

**Honest paths and fresh state.** A labelled value wraps at `/` and its continuation
rows keep the label indent; a sentence that names `press 1` is drawn only when the
current screen is not 1 (the no-op phrase is replaced by `on the Agents screen (1)`).
The Agent detail leads with the clone it has (name, branch, short commit, clean or
changed), never with `not a git repository` about the workspace root. Runs selects the
newest record when the screen opens and when that newest record becomes terminal —
including a run whose id was already on disk as `running`, so the id did not change
when it ended. After a
reopen the Forgejo role field reads `last check: administrator (5 min ago)`, `last
check: failed (transport) (5 min ago)` or `not checked this session` — never the
configured default as if it were a check result. The first-run overlay owns its footer:
`y records it / n not now / esc not now`, never `q quit` or a screen key.

**Density and separation.** Agents, Instruction and Credentials carry the facts for the
moment, one status line and `? all keys`; the long custody and `systemd` explanations
live behind `?`, and the acknowledgment record behind `R`. The model's reply is set
off from the product's own notes by a dim rule above it and a dim `end of the model's
reply` rule below it.

**After a run.** When the run's own repository state proves a branch was pushed — git's
reflog for the changed remote-tracking ref has, as its **latest** entry, exactly
`update by push`, stamped during the run, and that entry's new object is the local
branch's commit — the Runs note names `P` (Forgejo screen 4) as the next step. A
command that merely says `git push` makes no claim; neither does a fetch or pull that
lands the same commit (its latest reflog entry is not exactly `update by push`), an
absent or unreadable reflog, or a repository the run cannot read. A push followed by a fetch or
pull that moves the ref again makes no claim, because the latest entry is no longer
the push. A hand-written `git update-ref -m "update by push"` is indistinguishable from
a push by this rule and is not detected. The rule proves a push that git's reflog
recorded as a push, and nothing more. When
`pull-request.txt` is absent, `P` uses the newest run's final reply: its first
non-empty line is the title and the whole reply is the body. The Instruction screen
says plainly that the agent has one tool,
`shell`, and no pull-request or issue tool; `P` is the person's key after the run.

---

## Installer output (before any screen)

| State | Shows | Must offer |
|---|---|---|
| installed, install directory on `PATH` | `installed <path> (<tag>)`, `run it with: gewu-multiverse` | a closing line that resolves when typed |
| installed, install directory **not** on `PATH` | `installed …`, a `note` about `PATH`, `run it with: gewu-multiverse` | the closing line must be the command that works: the absolute path |
| `--help` | the option list, and a closing line naming the one file the program is and saying that removing it is the whole uninstall, with the configuration and state directories left as they are | where the binary the installer left lives, and how to take it away or keep it, without reading the repository or remembering the installer's own line (F8) |

## Agents (`1`) — the hub

| State | Shows | Must offer |
|---|---|---|
| first run, acknowledgment not recorded | the risk overlay (see below) over the Agents screen; the sample-configuration line behind it names the file but not a key, so the overlay's `n — not now` is the only meaning `n` has there | one legible overlay; `y`/`n` both act; `n` leaves the list visible; the add route and the term-recording route appear on the list after the overlay closes, not under it |
| nothing configured | empty list; no agent; a note naming the configuration file; the list draws only the rows it needs, so the note area gets the rest of the height even on a tall terminal; the note says plainly **there is no scheduler** — a run starts when `s` is pressed, and `m` keeps a run alive as a user unit, not on a timer | a form, by name: “press `n` to add an agent”. `n`/`a` must answer, and the empty table never takes rows the note needs |
| partially configured | one or more rows: name, model, concurrency, supervision, state; the selected row carries `>`; the table takes only the rows its agents need, so the note area is not crowded out on a tall terminal; a failure state shows its class whole (`failed (server_error)`), not a shortened word | each row readable **and present**; the newest run's outcome, with a **non-success** named when it is the newest terminal record — `failed`, `timed_out` and `stopped` all show their reason here, not only on Runs (C-3); **one rule for reasons** (w45 F3): every reason about an agent's run is drawn for **that agent's row**, naming the agent, whether or not it is selected — the non-success reason, the start-failure reason, the unreadable reason and the changed-history sentence are one rule, not four, so a list scan answers “why is that row like that?” for every row; an older failure is not called “the last run”, and it still stands as `last failure` on the Agent screen; when a record the hub had is gone (deleted by hand) the hub says so once and calls the newest record left *the newest run record left*, never “the last run”; a record that is **present but unreadable** is `unreadable`, the same word the Runs screen uses, never `idle` — the word for no run (W41-2, Z-3), and its reason under the same row rule (U2); `concurrency` and `max turns` defined in the `?` key guide; the table's name and model columns flex to their content up to a 32-column cap, the state column keeps its class width first, one column of right padding stands before the border, and trailing slack follows `state` rather than being inserted inside a column (Z-2, K-4/K-17) |
| a new agent reuses an agent's name | the new row's state is built only from records written by the agent now under that name, so a freshly created agent shows `idle`, `no run record on this machine yet`, and no inherited failure, newest log or transcript; a record that predates it is marked `*` and belongs to the earlier agent | no inherited history: the earlier records stay on disk, unchanged and recognisable as an earlier agent's, and no record is migrated or rewritten. The product writes a small `previous-runs` note in the agent's state directory listing the run ids that existed before this creation; the note is not a run record and creates no run. The note is written only after the new agent's configuration and key are saved, in one atomic rename, so a refused add leaves no boundary behind. An agent created before the note existed keeps every record under its name, because the product cannot invent a boundary it never recorded |
| a record of the risk acknowledgment exists | the note area names the date and the version the record itself carries **and the version now running**, each labelled, and says `R` reads the terms again; the sentence names the **configuration** the record belongs to and the machine it was made on — `this configuration's record was made on this machine`, or `this configuration's record was made on another machine … this machine has not recorded a confirmation`, or says the record does not name a machine — so a directory copied to a second person on the same machine cannot present the first person's confirmation as that second person's (O3); a record made on **another machine** also says what to do about it — that starting is still allowed under that record, and that removing the record file and starting again records this machine's own confirmation (w45 F2: know / do not know / what to do); the line also says the record is a **plain file, not signed**, so anyone who can read the directory can edit it (C-4); when the configuration has since been opened by a **newer** version (the `last-seen` record), a sentence says so — `this configuration has been used by version X, which is newer than this build …` — and the acknowledgment line adds `used by version X since <date>`, so going back to an older build is not silent, while a build at least as new as the record says nothing extra (U3) | what a second person on a used configuration directory agreed to, when, under which version, **and on which machine**, compared with the version in front of them, before anything starts; `R` shows the terms **read-only**, so reading them does not rewrite the record (C-3, F3, W32-6, O3); `R` also states plainly what the record can and cannot prove — it is a plain 0600 file, not tamper-evident, and its digest shows only whether the terms text has changed since it was made (C-4) |
| ready (idle) | rows with `idle`; the concurrency count and the note about `tui` | the keys that matter (`p`, `s`, `c`, `e`, `d`) visible or one `?` away |
| many agents (more than fit) | a window of rows whose title reads `agents a–b of N — ↓/↑ more`; the selected row is always drawn and marked `>` | no agent is hidden **silently**, and the selection never moves onto a row that is not drawn |
| removing | a confirmation over the list | `y` removes the agent from `config.toml`; `n` keeps it; the files left on disk are named, or the confirmation says it had none yet. An agent with a run in flight is **refused** with its run named and the stop key (`x`) offered, so removing it can never orphan the run. A name removed here is not an identity: creating a new agent with that name starts a new lineage, and the earlier records stay the earlier agent's (the `previous-runs` note in the state directory). |
| running | `running (Ns)`; `Running: 1 of N allowed` | the run id and the stop key named; the Runs screen agrees; the outcome vocabulary is one word per outcome on every screen, read from the single vocabulary map, so the success word is `succeeded` (not `exited 0`) and the timeout word is `timed_out` on the hub too, never `timed out` (W39-2) |
| waiting for a slot | `waiting (1 in line)`; “is waiting its turn: …” | the slot count and the line position; no error |
| a waiting start is called off (`x`) | `start called off`; the note names the agent's last **start** as called off | a start that never occupied a slot is a start, not a run: the row never overwrites the agent's real last run's outcome, the reserved start id the start never wrote is never checked, and a real run record the hub had observed and that is then deleted is still reported as gone while the call-off is held |
| stopped by `x` | `stopped`; “its process group was signalled and is proven gone” once the record is closed | the stop is confirmed, in the past tense, so the status and the row agree; the agent can be started again |
| failed | `failed (class)`; the last reason in the note area | **why**, on this screen: the class and the run's own reason. A transport failure names the endpoint URL and the way to correct it (remove and re-add the agent with the right address, or fix the configuration and press `r`); a failed declared step names its command, its working directory and an output excerpt, not only its exit code. A reason read from a record is **history**: when a path in it is not the file on the screen now (for example a configuration directory that has moved), the sentence says the path is the run's own and names the file the configuration points at now, and claims nothing about whether the complaint still holds (W-1). A `server_error` names the product's own next step where the reason is drawn — select the agent, then `s` to try again or `p` to test the endpoint — without promising that a retry will work or starting anything. A record that belongs to a removed agent or an earlier agent with the same name names its `T`/`o` history keys instead of a route it cannot follow. |
| error (config unreadable) | `no agent list: <reason>` | the reason in the product's words, the **real** class (a syntax error is "not valid TOML"; an unknown field is "not a valid configuration"), and a way back named with the error (`press r to read it again`, or the form); a secret-like key's section is named the way the file writes it (`[forgejo]`, `[[agents]]`), never a dotted path the user did not type (F5) |

## Agent (`2`) — one agent's configuration

| State | Shows | Must offer |
|---|---|---|
| no agent | `no agent to show: …` | the same in-app path as the empty Agents screen |
| configured | name, model, endpoint, max turns, instruction, **concurrency**, run state, supervision, unit/restarts, agent files, workspace, **repository**, newest log, endpoint test, identity; while an endpoint test is **in flight** the field says `testing — started …` and the previous result is not drawn (S-2) | every word the user needs defined here or on the screen that introduced it; no `identitynot`; when the window is too short for every field, the panel **title** says more is below, so a field is never dropped silently (a path breaks at its `/` separators, never inside a file name, and continuation rows keep the label indent), and the panel scrolls with `[`/`]` or the arrows so the fields below the fold are reachable; the identity line is the same shape every surface uses — `<login> <email> as \`<agent>\` (<mode>)` (F6); the endpoint field names its expected shape with the full-URL example `https://host/v1/chat/completions`, and `E` opens one bordered editor for the selected agent's endpoint, model id and model key without removing it; a refused URL keeps the editor open with its reason under the URL row; a workspace that already has history is not only a path: the `repository` field **leads with the clone** (its name, branch, short commit, age, clean/changed) when the product's `g` clone is at `workspace/<repo>`, and otherwise names the branch, the short commit and its **age**, whether the tree is clean or how many paths changed, and the repository's **configured** local git identity — what the next commit would carry, not who authored the commits already there (w46 F4); what the product cannot read (no repository, no git, no commit yet) is said plainly; when the workspace root is not itself a repository but holds the clone the product's own `g` made (`workspace/<repo>`), the first words are the clone's name, branch, commit, age and clean/changed state; the sentence never opens with `not a git repository` (R-2), and when **no Forgejo identity is recorded for the agent yet** the sentence names that state and says the product therefore cannot tell whether the git identity is the agent's — never `not the agent's` against an empty expectation, while a recorded identity that matches reads `the agent's` and one that differs still reads `not the agent's` (W-2, W2-A); the endpoint test's **class and cause**, in the product's words, wherever the result is shown (the status line carries both, so a short window cannot reduce it to the class); the endpoint test is **stamped** (`tested just now`, `tested 2 days ago`) — one age ladder, an instant `just now` then seconds then minutes, hours and days (W-6) — and the stamp is **persisted** in the endpoint-test record beside the acknowledgment file, so it survives a restart and ages instead of vanishing with the session, while an agent with no test at all says `not tested yet — press p` rather than drawing no field (LIB-2); a test result belongs to a **machine and a time**, so the record carries the machine that made it and a result from another machine says so (`tested on another machine`), a record that names none says the product cannot tell, and neither is drawn as this machine's own (W32-2); and a result about an endpoint or model that has since changed is marked `not current`, so a stale success cannot read as today's; a `newest log` whose file is gone is marked `gone, the record has no log` rather than shown as a path, and an agent with no record **on this machine** says `no run record on this machine yet` — the run records are machine-local, and a copied configuration must not tell a person their agent never ran (W32-3); the **newest log and the newest transcript each show their size on disk**, so the files that grow with every run are visibly growing (L-3); a file that is **gone** and a file that is **present but unreadable** are not the same fact: both screens say `gone, the record has no transcript` / `gone, the record has no log` for a deleted file (W38-3) and `— the record could not be read: <reason>` for one that is there but cannot be read — a **directory** at the log's or the transcript's path is never drawn as a readable file with a size, and `o`/`T` name the same reason instead of `is not there yet` (W38-1, U1); the transcript's content error is read rather than swallowed (W38-1) |
| removing | the same fields, with the removal confirmation over them | `d` removes the agent (after asking); the files left on disk are named — **both** the agent directory and the run records under the state directory — and the credentials file is named with the `D` that deletes the leftover secret, so a mistyped agent is not a dead end. A newly created agent under a removed agent's name starts from `idle`: the earlier records are marked and named as the earlier agent's, not adopted as this agent's newest run. |
| failed | run state `failed`; the failure class and the run's reason | the reason stored, not a transient footer line. A `server_error` carries the same next-step sentence (select the agent, then `s` retry or `p` endpoint test) as the hub. |
| timed_out or stopped | run state `timed_out` / `stopped`; the run's own reason | the reason named on this screen too, in the `last run` field, not only on Runs (C-3) |
| a waiting start is called off | run state `start called off`; the `start` field names the id the start would have had; `last start` says it was called off before a slot was free | the call-off named as a start, distinct from the newest log/transcript and the `last failure`/`last run` fields, which stay the real run's |
| systemd, unit start failed | run state; unit; restarts | the `systemctl` reason, recoverable |
| systemd, ready | supervision `systemd (user unit)`; unit name; restarts; the user manager reachable | what `systemd` changes *before* `m` is pressed; the confirmation that follows reads as a sentence (`... started the next time it is started`), with no `(s)` gutter; a `systemd` record the product itself wrote (which names no run) still says when the manager cannot be reached, so `systemd` + `idle` is never shown as if a start would work |
| systemd, the user manager is not reachable | supervision stays `tui`; the refusal names what is missing, says nothing was changed, and names the route to a reachable user manager (start a login user session, or run `loginctl enable-linger $USER` on a systemd host, then press `m` again); on an existing `systemd` record, a red line naming the manager and `m` | the switch into `systemd` is **refused before the mode record changes**, so `m` is never a one-way door; leaving an existing `systemd` record succeeds without the manager when no unit file is on disk, because there is nothing to stop |

## Instruction, steps and skills (`3`)

The `path` on this screen is the agent's configured `instruction` document, resolved
against the configuration directory. The Agent screen and every run read the same file,
so no screen shows a path the run ignores (P-3); the add-agent form writes the derived
`agents/<name>/instruction.md`, which stays the common case.

| State | Shows | Must offer |
|---|---|---|
| no agent | `no agent to show` + the config path | the in-app path to add one |
| empty instruction | path, `not there yet — press e to write it`; a file that holds only the guidance placeholder reads the same way, with the reason, and so does a document the person emptied — `not there yet — press e to write it; the file is empty`, because a run refuses a blank document as empty and the screen must not draw a byte count and a sends line for it as if it were written (w47 F3); a file that is **present but unreadable** is neither — it says `— the record could not be read: <reason>`, the same sentence the transcript uses, so a document that is there is never called absent (Z-3) | `e`, and one line of what a run sends |
| instruction written | path, **the size on disk and the size a run sends** (the guidance line named as dropped only when it actually was; otherwise the trailing newline named as the difference), a **read-only preview** of the instruction and of the declared steps commands, the explicit working directory of those commands (the workspace; a `g` clone sits one level below it at `workspace/<repository>`), `skills N on disk`; the `e`/`t`/`K` action line is above the preview so it is visible at 72×18; the preview starts directly under that line, draws every row the panel has, and has no `N more lines` cap | a preview of the instruction and the steps command, or a key that opens each; what a run will send, not only what is on disk — the instruction body is visible at entry at 72×18, and the steps commands are above the skill prose that may fall below the fold; one line says the agent has the `shell` tool only and no pull-request or issue tool |
| an interrupted edit | `an earlier edit of this instruction was not saved; the editor kept it in <file>` | the emergency-save file named, so the words are recovered, not retyped |
| steps | path, declared count, or `none — press t`; a steps file that is **present but unreadable** says `— the record could not be read: <reason>`, never `none` — the word for a file whose steps are an empty list (Z-3) | the template must not doubt the product (“until model calls exist” is false); `t` writes the template only when it is opened, and if the editor leaves it untouched the status says the file was **created**, not that nothing changed (H-2) |
| skills | `skills N on disk`, one `SKILL.md` per skill | what a *skill* is, and `K` must not hand an editor a directory; the `SKILL.md` is written only when the editor opens, and an untouched template is reported as created — never as “not changed” beside a count that went up (H-1) |

## Forgejo (`4`) — connection and role check

| State | Shows | Must offer |
|---|---|---|
| no token | `role token absent`; `admin token absent (identities are supplied)`; “no role check has been run yet — press c”, or, when a check ran on this machine and its stamp is persisted, “no role check has been run in this session — checked <age> on this machine — press c to check again” (or `another machine`, never a host name or machine id) | what a *role* is, in one line, before `c` is pressed; when a check was made before, the stamp it left (`role_checked_at_ms` + `role_check_machine` in the role-check record beside the endpoint-test record) so the fact survives a restart instead of vanishing with the session (W37-1); the screen label is neutral, while the record keeps the full mark; after a reopen the `role` field is a **result or an absence**, never the configured default dressed as a result — `last check: administrator (5 min ago)`, `last check: failed (transport) (5 min ago)` or `not checked this session` — and the persisted stamp carries the outcome as well as the time and machine. The outcome is computed from the check that just ran, not from the state before it, and changing the address drops the old stamp, machine and result together, so a check against the previous address is never printed against the new one |
| a check was refused | `role check failed (<class>) at <configured url>: <reason>`; a token that is an administrator's but is checked as `member` says so, and names `C` **only when an administrator token is configured** — otherwise it says the token is an administrator's and names the add form or `A` instead of a key that cannot work | the URL the user configured, led before the reason, and the account's real standing, not a bare class; the role label follows the token in use (configured role plus the effective administrator standing), and an administrator token supplied in the form is not demanded again with `A` |
| configured, unread | the URL and role | a sentence that says *not read yet*, not “could not be read” |
| the address is wrong or belongs to another machine | `url <the configured address>` | `u` opens a visible one-field prompt on this screen, prefilled with the address in force, and writes **only** `[forgejo] url` for every agent **in this configuration** — the configuration root is per-person, so this is the person's address, not the machine's — naming what changed and that `c` re-checks the role against it; the add-agent form's own address field says in its receipt when it moved the address for agents that already existed, instead of moving them silently (W32-1, W41-1) |
| the address prompt (`u`) | one field, **visible** (the address is not a secret and a masked one could not be corrected), saying that it applies to every agent **in this configuration**, and naming `ctrl-u` as the key that clears it | `enter` writes it, `ctrl-u` clears the prefilled field, `esc` cancels; the receipt names the new address and says `c` checks the role against it (W32-1, S-3, W41-1) |
| checked (one token) | the view, a window of repositories whose title reads `repositories a–b of N — ↓/↑ more` when the list is longer than the panel (or, when that cannot share the title with the overflow declaration, a `repositories a–b of N` line above the list); when there are no repositories the scrollable title carries `no repositories` instead of a range that cannot exist, and the windowed title carries no range at all; each `[private]`/`[public]`, what the caller may do; the check/action line and the definition of a role are placed above the list so `c`/`C` stay visible at 72×18; the key legend is two compact rows, each phrase keeping its object (`v agent token`), and the rows after the list are reserved so the legend is never what the list pushes off | which repository is selected, in text (a `>` marker), not only in colour; the selected repository is always drawn and no repository is hidden **silently**; the role label says `administrator` when the supplied token produced that view; a member check whose account is an administrator reads `administrator account — checked as an ordinary member`, names `C` only when an administrator token exists, and otherwise names `A`; neither repeats the host name or machine id |
| checked, an identity report on the tail at 80×24 | the panel with the identity report is drawn **once**, after the repository list, whenever the identity is settled and the role check has run, **including a session that did not settle it**; the status after `i` is a one-line receipt, not a second copy of the report, and the panel never draws the report half-wrapped, so a `]` reaches it whole (W-3) | when the window can show only part of the list, the line above it is truthful (`repositories 1–1 of 4 — ↓ more`), and the list or the scrolled form takes the remaining height rather than reserving it for prose; when the extra tail makes the windowed form too tall, the scrolled form is used with `more below — press ]` in the title and `[`/`]` reach every repository and the two-row key legend: the legend is **not** pinned in this state, but it is one `]` away and nothing is hidden silently (P-6); the report's creation time is derived from the stored second, so it ages (`created 3 seconds ago`, `created 3 minutes ago`) rather than forever saying `created now` (H-3) |
| checked, too short for the windowed form (below 80×24) | the same panel drawn whole — summary, `repositories 1–N of N`, every repository with its `>` marker, the identity report, the clone note and the two-row key legend — with `more below — press ]` (or `↑ more above — press [` once scrolled) in the title; the `c`/`C` action that made or refreshes this view is above the list | the same discipline as every other panel: `[`/`]` scroll the Forgejo panel too, so the whole repository list and the key legend are **reachable** below 80×24 and at the 72×18 floor, not merely declared; `up/down` still chooses a repository. The refusal never names a panel that is not on this screen |
| checked rows then `g` | the clone acts as the agent's identity, while the list came from the role token | a failed clone names the account it acted as and says the list came from the role token, so two different tokens are not confused; a destination that **already holds a git repository** is reported as kept — `no clone and no fetch was run`, its local git identity set — never as a clone or a bare `is ready` (F2); `g`, `P` and `U` act **only** on this screen — off it, `g` is the list's own top-of-list key, never a Forgejo action |
| both tokens present | one view at a time, or both | a way to compare the two roles **inside the app** (`c` / `C`); a failed `C` keeps the last good view and shows the failure beside it |
| a token is missing | `role token absent` / `admin token absent` / `failed (no_role_token)` (the missing role token, its own class beside `no_agent_token` and `missing_admin_token`, never the class `credentials`, which names a credentials file that cannot be read — F-3) / `failed (no_agent_token)` (which opens `the agent token for \`<agent>\` is absent`) / `failed (missing_admin_token)` (which opens `no administrator token is configured, so only the configured role can be checked — press A on the Credentials screen to add one`), and a refusal a user acts on names the product's own keys (`press v on the Forgejo screen`, `press A on the Credentials screen`), never a file and a TOML section | a one-field, masked prompt (`v` for the agent's own token, `A` for the administrator token) that writes the credentials file, never an editor |
| `not_admin` | the exact refusal | keep it; it is the model |
| identity not settled | `agent identity  not settled for \`<agent>\` — press i; v supplies the token` (with an administrator token, `press i for \`gewu-<agent>\`; the account is created and managed`) | what `i` will change, before it creates an account, **whose** identity it is, and the `v` prompt that supplies the token `i` needs; the sentence finishes where it says what supplies the token (`v supplies the token`), never a clause that stops mid-thought (C-2), and the field names the agent so a screen a second person reads never follows the hub's selection in silence (F1) |
| identity settled | `identity gewu-<agent> <email> as \`<agent>\` (managed|supplied) — commits, pull requests and issues act as this account` | the login, the email, the agent **and** the mode persistently, in the one shape every surface uses (F1, F6); the sentence says what the agent does with the account: commits, pull requests and issues; when the last identity check failed, the failure is **joined to this line** (`— the last check failed (<class>): <reason>`) and not drawn again on the tail, so the panel has one verdict about whether the identity is usable now (W32-5) |

## Credentials (`5`)

| State | Shows | Must offer |
|---|---|---|
| ready | file path, the mode the file **actually has now** (`0600` when it is right; a widened file shows its real mode and the refusal below, which says it is refused until the product holds it back on the next re-read), the redaction promise, and **one status line**; the long custody, machine and copy explanation is behind `?`, not printed as policy prose (F6/K-20) | which secret **names** are present (`model key for…`, `Forgejo role token`), never a value; `k` sets a model key and `f` the role token, masked — the screen-specific action row is visible at 72×18 and 80×24 and ends `? all keys`; a refused write replaces the status line with the refusal, so the footer and the prompt agree (F-2) |
| after a removal | the removed agent's key, marked `(no agent with that name is configured; the credentials file holding it was last written <when> — that is the file's own time, and writing any secret moves it)`, and `D delete the leftover secrets of agents no longer configured` on this screen's own legend beside `k`/`f`/`A` | know that a listed secret belongs to no configured agent, **and what the file's last write was**, so a tidy-up is not a loss (C-4); the sentence says the time is the file's own and that any write moves it, so a write about another secret never silently restates what the note seems to measure (w46 F5); delete it: `D` opens a confirmation naming the agents and removes exactly those entries, never showing a value; the key is offered where the leftover is shown, not only on a help page |
| a secret is missing | `present  nothing yet — no model key and no Forgejo token`; a refusal elsewhere — a key that is missing **or** one the endpoint refused (`bad_key`, HTTP 401) — names this screen's own keys and the selection step that lands on the named agent (select `\`<agent>\`` on the Agents screen (1), then press 5 and k on the Credentials screen), never a file and a TOML section | a key on this screen to supply it (`k`, `f`, `A`), never a file to edit; the `k` prompt writes the key of the agent **selected on the Agents screen**, so a refused key for a non-selected agent names that selection step and is truthful to follow, while a refused key for an agent the configuration no longer lists promises no route and says so; a refused key names the route without printing a value, and a historical run's own reason stays history |
| not ready | `the credentials file is not ready: <reason>` | the reason, and that no value is ever printed. A file that cannot be read is **not** reported as empty; a file that is **not there** says so (`the credentials file is not there yet — press k, f or A on this screen to write it`) rather than naming a creation failure for a writable directory (R-6), and any of `k`/`f`/`A` puts the file back with mode 0600 in the same session (R-1) |

## Runs (`6`) — recent runs

| State | Shows | Must offer |
|---|---|---|
| empty | “No run has been recorded yet…” | where records will appear, and how a run is started |
| a run in progress | `running (<elapsed>)` (not `unknown start`); the transcript carries a `model_request_sent` written **before** each call, and the log says `waiting for the model`, so a wait is not a wedge | the start time from the run's own opening transcript record (not `plan.json`, which carries none), and that the run has not ended. **Z-1:** in the first moments of a run — before the opening record has landed — the row takes the state word and the start clock from the run the UI is watching: the same `running` the hub prints, and the instant carried by the run's own id, so the two screens never disagree while a run is in flight. **C-2:** a record that is present but **cannot be read** still has a start — the column falls back to the instant the run id itself carries, so `unknown start` is never shown for a run whose id names its start |
| finished | outcome with its reason and duration; the reason leads the note area, and the note area grows to hold it, so it is never the line that wraps off — but never below the list's own minimum of border, header and **three** runs (at 72×18 the list may collapse to one summary line instead, and then the records panel takes the remaining body); the model's **final reply is drawn in full** as readable, copyable text in the note area (not a JSON string, not an 80-byte excerpt), and the note area scrolls with `[`/`]` when it is longer than the window; a record that is present but **cannot be read** leads the note with its read error instead (`outcome: unreadable — the record could not be read: <reason>`), the same fact the hub draws (C-2); an empty list draws only its own rows, so the note area keeps the rest of the height | `o` the readable log (the full final reply, never truncated) and `T` the **raw transcript (JSONL)** as the secondary, machine-readable view, both named at 80×24 even beside a wordy reason; the pager's `q` named; a run left by a dead UI reports the time it actually ran, never `0.0s`; the paths sit below the offers and are reachable with `[`/`]`; the model's reply is set off by a dim rule above it and an `end of the model's reply` rule below it, so product notes cannot read as the model's words; when git's reflog for the run's remote-tracking ref has, as its latest entry, exactly `update by push`, stamped during the run, and that entry's new object is the local branch's commit, the outcome line names `P` on the Forgejo screen (4) as the next step; a command that only says `git push` does not, a fetch or pull that lands the same commit is not a push (its latest reflog entry is not exactly `update by push`), an absent or unreadable reflog makes no claim, a push followed by a fetch or pull that moves the ref again makes no claim, and a hand-written `git update-ref -m "update by push"` cannot be told from a push. A `server_error` reason carries the product's next step here too: select the agent, then `s` to try again or `p` to test the endpoint; a `transport` reason names the endpoint URL and the way to correct it; the product starts nothing by itself. |
| many runs | newest first, and the newest record is selected when the screen opens and again when that record becomes terminal — the id it already had does not change when the run ends, so the terminal transition itself moves the selection and the records offset, and the status line says the view moved to the run that just ended; a window whose title reads `recent runs a–b of N — ↓/↑ more`, where `N` is the **list total — every run record on disk**, never the fifty-record cap; the `↓ more` marker speaks for the window itself, so it is drawn while the list has more rows and not merely because older records exist off it (W39-1); the selected row carries `>`; a record whose agent the configuration no longer lists — or that predates the agent now configured under that name — carries `*` in the marker column and is listed like any other, so its transcript and log stay openable (R-5); the `started (UTC)` column says which clock it is; a run id too long for its column is shortened with an ellipsis, never clipped mid-token, and the outcome column keeps room for `succeeded, 0.0s` even at the 72×18 floor — taking it from the run-id column, which is already shortened with a marker — so no cell ends mid-value (T-3); the list holds the newest fifty records and, when more exist on disk, the note area says how many older records are not listed — the total is every record on disk, so the count and the rows agree and no two different `on disk` numbers sit on one screen (L-1, F4, H-4, R-5) | `enter` opens **that run's** agent, selecting it even when the hub's selection is elsewhere; on a removed or earlier agent's record it shows a plain sentence naming the record and `T`/`o`, never the current hub selection; the `not to the configured <name>` contrast is drawn only when that name is configured, so a removed agent's record gets the unlisted sentence alone; selection readable in text, and never on a hidden row |
| records on disk | `N run records on disk, <size> of file bytes` under the record path; when some belong to an agent the configuration no longer lists, or to an earlier agent with the same name, how many **on disk** and how many of them are inside the newest fifty listed above — the count and the presence of the `*` marker are stated separately, so the sentence never points at a marked row the window does not carry (W36-3); and that the product never deletes a record, so a person removes a run's directory to free its space; the size is the sum of **file bytes**, the same definition every size on every screen uses, and it is labelled in the binary units the arithmetic uses (`KiB`, `MiB`, `GiB`, and `B` below 1024), never `KB` over a value divided by 1024 (H-4, F-2) | the footprint and the cleanup expectation, on the screen that lists the records: where they live, how much room they take, and what is expected of a person (L-2, L-4). The `previous-runs` note is not a run record: it adds no record, changes no outcome, and is not counted in the run footprint. |

## Overlays

| Overlay | State | Must offer |
|---|---|---|
| first-run acknowledgment | shown once per configuration directory | own its rows (no bleed from the panel); `y`/`n`; the record path **at the advertised minimum too** (80×24), never a sentence that ends without its object; `R` on any screen shows the same terms again **read-only** when a record exists (the footer reads `esc closes this overlay; [ and ] scroll; the record is unchanged`, matching the overlay's own close line, `[`/`]` scroll the terms rather than closing it, and no key changes the record) and the ordinary `y`/`n` modal when none does (C-3, V-2); the read-only review is **titled for its own state** (`The terms you acknowledged — read-only`, not the first-run heading, w44 F3) and its box is sized to its content, so every terms bullet is drawn whenever the window can hold it and `more below — press ]` is true when it cannot (w44 F2); the read-only review also states plainly what the record **can and cannot prove** — it is a plain `0600` TOML file, the product does not sign it and anyone who can read the directory can edit it, and the digest it carries shows only whether the terms text has changed (C-4). The ordinary `y`/`n` modal is also what `R` opens when no record exists: `n — not now` is not the add-agent key, and the sample-configuration line under the modal never tells the person to press `n` while the modal owns it. At 80×24 the box is sized to its whole content — terms, the path and the record facts — with the answer row after the content; only at 72×18 does the body scroll, and the answer row stays last. |
| any overlay over a screen | the panel and the overlay | **a bordered overlay is sized to what it holds** — a fixed-height body may not declare `more below — press ]` while the scroll keys would not reveal the rest: either the box grows to fit, or the declaration is true of the window it names (w44 F2, the review overlay); the footer's status rows stay visible under the overlay — including the help overlay, which used to cover them — so a sentence about what just happened (a paste kept only its first line, a refusal, or the help's own "press PageDown" note) is never cleared away; and the footer's key legend never advertises `q quit` while an overlay owns the keys (there `q` is a letter in a field or a cancel), reading `esc closes this overlay` instead — the first-run acknowledgment included, where the footer reads `y records it once   n not now   esc not now` and never a screen key. A **paste** under an overlay is not dropped in silence either: the status says what owns the screen and how to reach a field (F4) |
| add-agent form (`n`/`a`) | opened from the Agents screen | name, endpoint, model, key, Forgejo URL, **role** token and a field saying which role the token is (`member` or `admin`) — every field drawn at 80×24; the introduction is compact and the endpoint label carries the full-URL example `https://host/v1/chat/completions`, so at 72×18 the first two or three fields are visible and named, and `blank Forgejo fields use the shared connection` is said once, with the body windowed so the cursor's own row is always drawn and the overflow title names `tab`/`↑`/`↓` rather than `[`/`]` (a bracket is data in these fields); writes the two files; the model key is **required**, so the agent the form creates can run; a token marked `admin` is written as both the role token and the administrator token, so the product never demands the same administrator token again with `A`; the form says the agent's own token is separate and is supplied later with `v`; the keys never echoed; `enter` moves to the next field and writes the form **only from the last field**; `ctrl-s` writes it from any field, so a stray `enter` in a middle field never submits an incomplete form and the next paste cannot land in the wrong field; on a refusal the cursor goes to the field the reason names, **the box draws its own reason on the row immediately after the field the reason names** (not the cursor's row), so the reason and the field are read together and the reason's row cannot be the row the window drops (C-1, w45 F1, w46 F1/w47 F2); the reason is a receipt of that refusal and **the first key that edits a field or moves the cursor withdraws it**, so a stale reason is never read under a field it is not about (w46 F1, w47 F2); **the `ctrl-s writes it / esc cancels` action line is in the panel at every size** (including 80×24); a masked field shows how many characters it holds; a bracketed paste keeps the first line, names the rest as unused when content was dropped (a trailing newline or whitespace is named as ignored), and **replaces** the field instead of appending; when the form's address field differs from the address in force and other agents exist, its receipt says the form **moved the Forgejo address for every agent** in this configuration, and names `u` as the place to edit it on its own (W32-1, W41-1). An invalid name is quoted back in the refusal (`cannot use “Scout” for an agent: it …`) with the rule text and the typed fields preserved; a value too long to quote is shortened with an ellipsis, so the reason stays readable. |
| secret prompt (`k`/`f`/`v`/`A`) | Credentials (`k`, `f`, `A`) and Forgejo (`v`, `A`) | one field, masked, showing how many characters it holds (`1 character` in the singular); writes the named secret to the credentials file, naming the agent it belongs to when the secret is per-agent; never a value on screen; a bracketed paste keeps the first line and replaces; on refusal (an empty value, a widened file) the box holds the reason **and the status line holds the same refusal**, replacing the paste sentence (F-2) |
| removal confirmation (`d`) | any configured agent | `y` or `enter` removes it from the configuration, `n`, `esc` or `q` keeps it — the destructive key is named in the box and **pinned at the bottom while the body scrolls with `[`/`]`**, so a floor window reveals the named files and never scrolls the answer away (the overlay sweep found the answer line was the row a 72×18 window dropped in silence); `q` is never the destructive one; the files left on disk are named **only when they exist**, and the run records under the state directory and the credentials file are named too, with where to delete the secret |
| leftover-secret delete (`D`) | Credentials, when a secret's agent is no longer configured | the agents are named (never a value); `y`/`enter` deletes exactly those entries from the credentials file and says what was deleted; `n`, `esc` or `q` keeps them |
| key help (`?`/`h`/`F1`) | any screen | every key, in groups, with one line of what it does; a **scrollable window** whose title reads `keys a–b of N — ↓/↑ more` when the window is short, so every key survives 80×24; it scrolls with up/down (and `j`/`k`), a page with PageDown/PageUp, and Home/`g` or End/`G` to either end; `[`/`]` scroll a panel or note area and `D` deletes a leftover secret; a key it cannot use says so instead of being swallowed; the `Moving:` line names `j`/`k` beside `up`/`down` and a `Jump:` line names `Home`/`g` and `End`/`G` (with the Forgejo `g` exception), so a key that moves the selection is never invisible (w46 F3, w47 F4), and `i` sits with the agent keys it acts on — it settles the **selected agent's** identity from any screen — instead of under `Forgejo:`, which is read as a screen heading (w47 F5). The risk line follows the state: with no record it says `R reads the terms; y records the acknowledgment`, and only with a record does it say `R read the acknowledged terms again`. |

## Hand-offs (editor, pager)

| Hand-off | State | Must offer |
|---|---|---|
| editor, `$EDITOR` set | the user's own editor | a confirmation naming the program before it opens, with `enter` to open it and `esc` to cancel — the same confirmation the unset case uses, because a status line is erased the moment the UI steps aside for the editor (N-1); an **interrupted** editor (killed before saving) returns with a sentence, not `code None`: it says the editor was interrupted, names the emergency-save file when one is there (`an earlier edit was not saved and the editor kept it in … — open that file to recover`), and ends with the return clause `the keys are the product's again` (R-4) |
| editor, `$EDITOR` unset | `vi` (or `nano` when present) | the program named, and how to leave it — the overlay gives `vi`'s `:wq`/`:q!` and `nano`'s `^X`, and the `nano` advice is true whether or not the editor asks to save (`^X leaves; if it asks, Y and enter saves and N does not`), because `nano` with an unmodified buffer leaves at once and a `N` typed afterwards belongs to the product (F5); **the hand-over is visible in the after direction too**: every edit and view outcome ends by saying the keys are the product's again, so the return is never silent (T-2); the example that shows how to change editor names an editor **other** than the one already chosen, so following it changes something (V-3); the sentence's verb agrees with its subject (`the declared steps … open in`, `the instruction … opens in`) (F6); an honest “not changed” when nothing was saved, and an honest “created” when the hand-off itself made the file the screen beside it now counts, with **the file's own name** in every hand-off status (`steps.txt`, `SKILL.md`, `instruction.md`) (H-1, H-2, W-4); backing out of the confirmation creates nothing |
| editor on skills (`K`) | a scratch `SKILL.md`, not a directory | what a skill is and the file name; the file is written only when the editor opens, and an untouched template is reported as created, with the consequence named (a run installs every skill under `skills/`) (H-1) |
| editor on an instruction | a file whose only guidance is one marked placeholder line | **the product's own placeholder text** is dropped from the document's first content line before a run sends the document — including when the person typed into the line the editor opens on, so the guidance is never sent glued to their sentence — while a copy of the placeholder later in the person's own document is kept, and the user's words around it are not deleted (T-1); an emergency save left by an interrupted editor is named before the hand-off |
| pager (`o`, `T`) | the user's own `$PAGER` | the path shown before it opens, and how to leave it (the pager's own `q`), because `esc` is this product's way back but not the pager's — and `esc` there can begin a sequence that swallows the next key, so the sentence names that trap and says to press `q` again; reading leaves the person's `HOME` as it found it (H-6) |
| the state a hand-off program keeps | each program the hand-off spawns | a state location of its own for every program the product chooses: `less` a history file at `/dev/null`, `nano` an `$XDG_DATA_HOME` in a scratch directory removed when it exits (so no `~/.local/share/nano`), `vi`/`vim`/`nvim`/`ex` `-i NONE` (so no `~/.viminfo`); a program the person chose with `$EDITOR`/`$VISUAL`/`$PAGER` that the product does not know keeps its own files, and is named in the hand-off sentence before it takes the screen — the product does not pretend to know what it writes (F7) |

## Signals and abandoned runs

| State | Shows | Must offer |
|---|---|---|
| the UI is signalled | — | the runs it started in `tui` mode end with it: the child asks the kernel to end when its parent does |
| the UI is left with `q` while a run is in flight, or a start is waiting | one warning naming **which** runs stop and how long each has run — `leaving stops `ledgerlite-changelog` (run-…, 4 s) and `punchcard-changelog` (run-…, 2 s)` — as well as the waiting agents by name (`calls off the waiting start of \`release-notes\``), singular and plural as the sentence needs; the receipt after leaving names the waiting agents too | press `q` again to leave, stop the named runs and call the named waiting starts off, or any other key to stay; staying **replaces the warning** with a plain statement rather than re-asking the question, and the staying key is **consumed**, so an unbound key cannot erase the statement with `no key is bound to that`; the record of each ended run names the UI closing, not “on request” |
| the UI is left with `q` while only a start is waiting | one warning naming the waiting start that will be called off (`1 waiting start`, not `1 start(s)`), with its pronoun | the same second-`q` leave, or any other key to stay; the waiting start is called off, writes no run record, and the receipt after the terminal is restored names it (`called off 1 waiting start`) beside any stopped run |
| the UI is reopened after a run was left behind | the agent as `stopped`, never `idle`; the run's duration is the time it actually ran | the run's own record is closed as `stopped` with a reason, so a second run of the same agent never overlaps an abandoned one, in the same workspace |
| the UI is reopened after a waiting start was called off by leaving or `x` | the agent's real newest run record, or `idle` when it has none; no sentence claims the called-off start ran, because a start that never occupied a slot writes no run record | the product does not invent a called-off history it did not persist: the row is the agent's real state, and a later run starts cleanly. An explicit `x` keeps its in-session `start called off` state and reason; leaving names the call-off in its warning and receipt |
| the UI is reopened after a run finished | the agent's newest terminal outcome, its reason and its elapsed time | the hub seeds each agent from its newest terminal record, so a restart never shows `idle` for an agent whose record says `succeeded`, `stopped` or `failed` |
| the configuration is re-read (`r`) after an agent was removed by hand | the removed agent's run is stopped, and the note names it once, in the note area | the run does not stay counted but invisible; what the footer counts is what a screen can show; the run's own record names the re-read, never “on request” |
| the configuration is re-read (`r`) after the run records were deleted | the hub drops an outcome whose record is gone; the hub and the ledger agree; when a cached outcome is dropped the hub says a record it had is no longer there, and an older record left is not called “the last run”; the sentence is drawn as the **first** line of the hub note area, above the acknowledgment line, the running line and the standing hazard/limits prose, so a short note area cannot hide the one sentence that explains the changed history (W38-2); a record that is **there but unreadable** is a different fact and is named as such (`is there but could not be read: <reason>`), not as one that is gone (W38-1) | a cached outcome never outlives the record it came from, and a person whose history was edited by hand is told |
| a start fails before a run ever runs (systemd refuses, the process cannot spawn) | the run state is `failed` with the start's reason, not `unknown start / no terminal record` | the record opened for the start is closed as `failed`, so the next `s` is not blocked by a run that never was |
| `^C` | — | a control chord is never a plain-letter binding: `^C` asks the same question `q` asks when runs are in flight (naming the runs and the way to stay), and a second `^C` leaves and stops them; with nothing running it leaves at once. Inside a hand-off (editor, pager) `^C` belongs to the user's own program: the product catches the signal for itself, so the hand-off's own exit returns to the product instead of killing it and stopping the runs, and when it regains the terminal it asks the same leave question |

## Every screen — footer

| State | Shows | Must offer |
|---|---|---|
| wide terminal | the status line and the key legend | the legend may be long; at a width that can hold every key it draws every one of them and still ends with `? all keys`, and only when the running count is long enough that they cannot all fit does it end with `… more — ? all keys` and leave the rest to the full list (Z-4) |
| a key that is local to one screen, pressed elsewhere | the status line | the refusal says the key is not bound **on this screen** and, when the key does act somewhere, names that screen and what it does there (`u` sets the Forgejo address on the Forgejo screen (4)), so a written procedure that names the wrong screen leaves the person a route rather than a dead end; the help overlay's headings and the footer legend state where each key acts, and the legend lists only keys that act on every screen (w44 F1) |
| every screen, every state | a footer clause `Running: N of M`, and `· K ended` once a run has ended since this UI started; `K` is a count over the newest fifty records, so whenever older records exist on disk it is printed `· K+ ended` — a lower bound, never the window presented as exact, including while a run inside the window is still in flight; the word is `ended`, not `finished`, because the count includes runs that timed out or were stopped and `finished` read as `succeeded` (W36-2, W39-3, L-5) | the running count and what has ended, wherever the user is, not only on the hub |
| 80×24 | the status line and the legend | the legend must not silently drop keys: it clips at a key boundary and ends with `? all keys`, and the way back (`esc agents`) is among the shown keys |
| any status longer than the width | the status line, grown past its usual rows when it must be | the footer takes the body's spare rows for the status (never below the body's minimum), so a reason, a path or git's own `fatal:` line is read whole at 80×24, never clipped |
| a window smaller than 72×18 | one line: “this window is too small to show the product; make it at least 72×18” | that line, instead of clipped panels, a clipped modal or a clipped help overlay |
| exactly 72×18 (the advertised minimum) | the product, with at least one agent row drawn and the count in the title when more exist, and the tab bar with all six names whole (`6 Runs`, not `6 Ru`) | a minimum the product calls usable must show a real row, not a header over nothing, and must name every screen it offers |
