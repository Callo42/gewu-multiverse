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
panel puts `more below; enlarge the window` in its title (always drawn), and the two
borderless note areas (Agents, Runs) reserve their last drawn row for the same
sentence. A bordered panel never draws **half a wrapped line**: whole logical lines are dropped
from the bottom until the rest fits, so a sentence never ends at the box edge — the
`more below` declaration and `[`/`]` are what carry the rest. While the first-run
acknowledgment modal owns the screen, the note area behind it draws no overflow
declaration at all: the sentence would point at rows the modal hides (W-5), and it
returns the moment the modal closes.
A surface scrolled away from its own top carries the sibling marker —
`↑ more above` in a bordered title, `↑ more above — press [ to go back` as the first
drawn row of a note area — so a scrolled panel never hides its own heading in silence.
**Every modal and overlay is a bordered surface too**: its body drops whole logical
lines from the bottom until what is left fits, it keeps its own answer line (the key
that answers it) as the last drawn row whenever that line fits, and it puts the same
declaration in its title when anything was dropped — so a modal at the 72×18 floor
never shows a sentence that stops mid-thought without saying it was cut, and its
answer line is never the part that falls off (F-1). The declaration is the part that
must survive: when a panel's title carries more than
its name (the Forgejo panel carries its repository range) and both cannot fit, the
screen name is shortened and then the declaration is shortened, and only if neither
can hold both does the range move to a line above the list. A column that shows an
agent's **name** is sized to the shortest width that keeps every configured name
distinct; a name that still cannot fit is shortened with an ellipsis that keeps both
ends, never clipped mid-word with no marker, and the failure class in a state column is
shortened to a word that fits whole. A login and its email are one fact: their
separating space is a no-break space, so a wrap at the line edge can never eat it and
leave `someone-else` glued to `<someone@else.invalid>` (N-2). **The scroll offset belongs
to the screen it was made on**: every way of changing screens (`1`–`6`, tab and
left/right, `esc`, `enter` into an agent) returns the new screen to its own top, so a
screen the user never scrolled never opens at another screen's offset. **The status
sentence belongs to the screen that made it**: the same screen change clears it, so no
sentence about a panel, a key or an action outlives the screen it was made on (C-1);
what must persist across screens is drawn from the state that owns it — the note areas
read the records, the acknowledgment line reads the record — never from the status line.
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

**A paste is a paste.** The terminal frames a bracketed paste as one event, and the
product never replays it as keystrokes. A one-line field keeps the first line, names
the rest as unused, and **replaces** the field rather than doubling it; with no field
open the paste is dropped with a sentence naming where it can land (`press n for the
form, or k/f/v/A for a secret`); under an overlay that owns the keys the status says
what owns the screen. A byte stream the terminal does **not** frame as a paste (a
program injecting raw bytes) is indistinguishable from typing, and the product is not
asked to answer for it (F4).

**One custody for everything the product writes.** Every directory the product creates
under its configuration and state roots is held to mode `0700` — the configuration
root, `agents/` and each agent, skill and workspace directory, and the state root with
each agent, run and run-skills directory. Every file it writes is `0600` — the
configuration file, the credentials file, the acknowledgment record, the instruction,
steps and skill templates, the plan, the log and the transcript. The one exception is
the `systemd` unit, which is `0644` because it holds no secret: it names paths only,
and the directory around it is `0700`. The Credentials screen states the credentials
file's real mode and the same custody for the files beside it (F-1, F-2). The custody is also **restored to a tree an older version left**: on every load the product holds its own configuration and state roots back to `0700` directories and `0600` files (not descending into `workspace`, whose clone is git's), so a run directory or configuration file an older version wrote is repaired instead of kept at its old umask mode (R-1).

The product's vocabulary is small and fixed: **agent**, **endpoint**, **model**,
**instruction**, **skill**, **steps**, **run**, **role**, **identity**, **credentials
file**, **configuration file**, **supervision**, **concurrency**, **max turns**. A
screen may use a word only after the screen, or a screen before it, has shown what it
means. `concurrency` (how many model requests one agent may keep in flight at once)
and `max turns` (the most model turns one run may take) are defined in the Agents note
area where they first appear. A transport failure is shown on screen in the product's
own words (`the endpoint refused the connection`, `the connection dropped`, `the
address could not be resolved`); the run's log and transcript keep the transport's
exact sentence, so the failure is still auditable afterwards. A count in a plain-text
line reads as a sentence (`1 turn`, `2 turns`, `0 repositories`), never a `turn(s)`
gutter; the JSONL transcript keeps its numbers.

Screens: **Agents** (`1`) · **Agent** (`2`) · **Instruction** (`3`) · **Forgejo**
(`4`) · **Credentials** (`5`) · **Runs** (`6`); the tab bar carries each number beside
its name, and every name survives whole at the advertised floor (72×18) — the bar
sheds its side padding rather than clipping the last name to a fragment. Overlays:
**first-run
acknowledgment**, **add-agent form**, **key help**. Hand-offs: **editor**, **pager**.

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
| first run, acknowledgment not recorded | the risk overlay (see below) over the Agents screen | one legible overlay; `y`/`n` both act; `n` leaves the list visible |
| nothing configured | empty list; no agent; a note naming the configuration file | a form, by name: “press `n` to add an agent”. `n`/`a` must answer |
| partially configured | one or more rows: name, model, concurrency, supervision, state; the selected row carries `>` | each row readable **and present**; the newest run's outcome, with a failure named only when it is the newest terminal record — an older failure is not called “the last run”, and it still stands as `last failure` on the Agent screen; when a record the hub had is gone (deleted by hand) the hub says so once and calls the newest record left *the newest run record left*, never “the last run”; `concurrency` and `max turns` defined in the note area |
| a record of the risk acknowledgment exists | the note area names the date and the version the record itself carries **and the version now running**, each labelled, and says `R` reads the terms again | what a second person on a used configuration directory agreed to, when, and under which version, **compared with the version in front of them**, before anything starts; `R` shows the terms **read-only**, so reading them does not rewrite the record (C-3, F3) |
| ready (idle) | rows with `idle`; the concurrency count and the note about `tui` | the keys that matter (`p`, `s`, `c`, `e`, `d`) visible or one `?` away |
| many agents (more than fit) | a window of rows whose title reads `agents a–b of N — ↓/↑ more`; the selected row is always drawn and marked `>` | no agent is hidden **silently**, and the selection never moves onto a row that is not drawn |
| removing | a confirmation over the list | `y` removes the agent from `config.toml`; `n` keeps it; the files left on disk are named, or the confirmation says it had none yet. An agent with a run in flight is **refused** with its run named and the stop key (`x`) offered, so removing it can never orphan the run |
| running | `running (Ns)`; `Running: 1 of N allowed` | the run id and the stop key named; the Runs screen agrees; the success word is the same one Runs uses (`succeeded`, not `exited 0`) |
| waiting for a slot | `waiting (1 in line)`; “is waiting its turn: …” | the slot count and the line position; no error |
| stopped by `x` | `stopped`; “its process group was signalled and is proven gone” once the record is closed | the stop is confirmed, in the past tense, so the status and the row agree; the agent can be started again |
| failed | `failed (class)`; the last reason in the note area | **why**, on this screen: the class and the run's own reason. A reason read from a record is **history**: when a path in it is not the file on the screen now (for example a configuration directory that has moved), the sentence says the path is the run's own and names the file the configuration points at now, and claims nothing about whether the complaint still holds (W-1) |
| error (config unreadable) | `no agent list: <reason>` | the reason in the product's words, the **real** class (a syntax error is "not valid TOML"; an unknown field is "not a valid configuration"), and a way back named with the error (`press r to read it again`, or the form); a secret-like key's section is named the way the file writes it (`[forgejo]`, `[[agents]]`), never a dotted path the user did not type (F5) |

## Agent (`2`) — one agent's configuration

| State | Shows | Must offer |
|---|---|---|
| no agent | `no agent to show: …` | the same in-app path as the empty Agents screen |
| configured | name, model, endpoint, max turns, instruction, **concurrency**, run state, supervision, unit/restarts, agent files, workspace, **repository**, newest log, endpoint test, identity | every word the user needs defined here or on the screen that introduced it; no `identitynot`; when the window is too short for every field, the panel **title** says more is below, so a field is never dropped silently (a path breaks at its `/` separators, never inside a file name), and the panel scrolls with `[`/`]` or the arrows so the fields below the fold are reachable; the identity line is the same shape every surface uses — `<login> <email> as \`<agent>\` (<mode>)` (F6); a workspace that already has history is not only a path: the `repository` field names the branch, the short commit and its **age**, whether the tree is clean or how many paths changed, and whose local git identity the commits carry; what the product cannot read (no repository, no git, no commit yet) is said plainly; when the workspace root is not itself a repository but holds the clone the product's own `g` made (`workspace/<repo>`), the sentence **names the clone** (branch, commit, age, clean/dirty) instead of reading the root as “not a git repository” (R-2), and when **no Forgejo identity is recorded for the agent yet** the sentence names that state and says the product therefore cannot tell whether the git identity is the agent's — never `not the agent's` against an empty expectation, while a recorded identity that matches reads `the agent's` and one that differs still reads `not the agent's` (W-2, W2-A); the endpoint test's **class and cause**, in the product's words, wherever the result is shown (the status line carries both, so a short window cannot reduce it to the class); the endpoint test is **stamped** (`tested just now`, `tested 2 days ago`) — one age ladder, an instant `just now` then seconds then minutes, hours and days (W-6) — and a result about an endpoint or model that has since changed is marked `not current`, so a stale success cannot read as today's; a `newest log` whose file is gone is marked `gone, the record has no log` rather than shown as a path; the **newest log and the newest transcript each show their size on disk**, so the files that grow with every run are visibly growing (L-3) |
| removing | the same fields, with the removal confirmation over them | `d` removes the agent (after asking); the files left on disk are named — **both** the agent directory and the run records under the state directory — and the credentials file is named with the `D` that deletes the leftover secret, so a mistyped agent is not a dead end |
| failed | run state `failed`; the failure class and the run's reason | the reason stored, not a transient footer line |
| systemd, unit start failed | run state; unit; restarts | the `systemctl` reason, recoverable |
| systemd, ready | supervision `systemd (user unit)`; unit name; restarts; the user manager reachable | what `systemd` changes *before* `m` is pressed; the confirmation that follows reads as a sentence (`... started the next time it is started`), with no `(s)` gutter; a `systemd` record the product itself wrote (which names no run) still says when the manager cannot be reached, so `systemd` + `idle` is never shown as if a start would work |
| systemd, the user manager is not reachable | supervision stays `tui`; the refusal names what is missing and says nothing was changed; on an existing `systemd` record, a red line naming the manager and `m` | the switch into `systemd` is **refused before the mode record changes**, so `m` is never a one-way door; leaving an existing `systemd` record succeeds without the manager when no unit file is on disk, because there is nothing to stop |

## Instruction, steps and skills (`3`)

The `path` on this screen is the agent's configured `instruction` document, resolved
against the configuration directory. The Agent screen and every run read the same file,
so no screen shows a path the run ignores (P-3); the add-agent form writes the derived
`agents/<name>/instruction.md`, which stays the common case.

| State | Shows | Must offer |
|---|---|---|
| no agent | `no agent to show` + the config path | the in-app path to add one |
| empty instruction | path, `not there yet — press e to write it`; a file that holds only the guidance placeholder reads the same way, with the reason | `e`, and one line of what a run sends |
| instruction written | path, **the size on disk and the size a run sends** (the guidance line named as dropped only when it actually was; otherwise the trailing newline named as the difference), steps, `skills N on disk` | a preview of the instruction, or a key that opens it; what a run will send, not only what is on disk — drawn at 80×24, above the skill prose that may fall below the fold |
| an interrupted edit | `an earlier edit of this instruction was not saved; the editor kept it in <file>` | the emergency-save file named, so the words are recovered, not retyped |
| steps | path, declared count, or `none — press t` | the template must not doubt the product (“until model calls exist” is false); `t` writes the template only when it is opened, and if the editor leaves it untouched the status says the file was **created**, not that nothing changed (H-2) |
| skills | `skills N on disk`, one `SKILL.md` per skill | what a *skill* is, and `K` must not hand an editor a directory; the `SKILL.md` is written only when the editor opens, and an untouched template is reported as created — never as “not changed” beside a count that went up (H-1) |

## Forgejo (`4`) — connection and role check

| State | Shows | Must offer |
|---|---|---|
| no token | `role token absent`; `admin token absent (identities are supplied)`; “no role check has been run yet — press c” | what a *role* is, in one line, before `c` is pressed |
| a check was refused | `role check failed (<class>) at <configured url>: <reason>`; a token that is an administrator's but is checked as `member` says so and names `C` | the URL the user configured, led before the reason, and the account's real standing, not a bare class |
| configured, unread | the URL and role | a sentence that says *not read yet*, not “could not be read” |
| checked (one token) | the view, a window of repositories whose title reads `repositories a–b of N — ↓/↑ more` when the list is longer than the panel (or, when that cannot share the title with the overflow declaration, a `repositories a–b of N` line above the list), each `[private]`/`[public]`, what the caller may do; the definition of a role shortens once the check has run, so the list gets the panel (more than one row at 80×24); the key legend is two compact rows, each phrase keeping its object (`v agent token`), and the rows after the list are reserved so the legend is never what the list pushes off | which repository is selected, in text (a `>` marker), not only in colour; the selected repository is always drawn and no repository is hidden **silently** |
| checked, an identity report on the tail at 80×24 | the panel with the identity report and the clone note after the list; the report is drawn whenever the identity is settled and the role check has run, **including a session that did not settle it**; the report is drawn **once**, on the panel — the status after `i` is a one-line receipt, not a second copy of the report, and the panel never draws the report half-wrapped, so a `]` reaches it whole (W-3) | when the extra tail makes the windowed form too tall, the scrolled form is used with `more below; enlarge the window` in the title and `[`/`]` reach every repository and the two-row key legend: the legend is **not** pinned in this state, but it is one `]` away and nothing is hidden silently (P-6); the report's creation time is derived from the stored second, so it ages (`created 3 seconds ago`, `created 3 minutes ago`) rather than forever saying `created now` (H-3) |
| checked, too short for the windowed form (below 80×24) | the same panel drawn whole — summary, `repositories 1–N of N`, every repository with its `>` marker, the identity report, the clone note and the two-row key legend — with `more below; enlarge the window` (or `↑ more above` once scrolled) in the title | the same discipline as every other panel: `[`/`]` scroll the Forgejo panel too, so the whole repository list and the key legend are **reachable** below 80×24 and at the 72×18 floor, not merely declared; `up/down` still chooses a repository. The refusal never names a panel that is not on this screen |
| checked rows then `g` | the clone acts as the agent's identity, while the list came from the role token | a failed clone names the account it acted as and says the list came from the role token, so two different tokens are not confused; a destination that **already holds a git repository** is reported as kept — `no clone and no fetch was run`, its local git identity set — never as a clone or a bare `is ready` (F2); `g`, `P` and `U` act **only** on this screen — off it, `g` is the list's own top-of-list key, never a Forgejo action |
| both tokens present | one view at a time, or both | a way to compare the two roles **inside the app** (`c` / `C`); a failed `C` keeps the last good view and shows the failure beside it |
| a token is missing | `role token absent` / `admin token absent` / `failed (no_agent_token)` (which opens `the agent token for \`<agent>\` is absent`), and a refusal a user acts on names the product's own keys (`press v on the Forgejo screen`, `press A on the Credentials screen`), never a file and a TOML section | a one-field, masked prompt (`v` for the agent's own token, `A` for the administrator token) that writes the credentials file, never an editor |
| `not_admin` | the exact refusal | keep it; it is the model |
| identity not settled | `agent identity  not settled for \`<agent>\` — press i; v supplies the token` (with an administrator token, `press i for \`gewu-<agent>\`; the account is created and managed`) | what `i` will change, before it creates an account, **whose** identity it is, and the `v` prompt that supplies the token `i` needs; the sentence finishes where it says what supplies the token (`v supplies the token`), never a clause that stops mid-thought (C-2), and the field names the agent so a screen a second person reads never follows the hub's selection in silence (F1) |
| identity settled | `identity gewu-<agent> <email> as \`<agent>\` (managed|supplied) — clones and pull requests act as this account` | the login, the email, the agent **and** the mode persistently, in the one shape every surface uses (F1, F6) |

## Credentials (`5`)

| State | Shows | Must offer |
|---|---|---|
| ready | file path, the mode the file **actually has now** (`0600` when it is right; a widened file shows its real mode and the refusal below, which says it is refused until the product holds it back on the next re-read), the redaction promise | which secret **names** are present (`model key for…`, `Forgejo role token`), never a value; `k` sets a model key and `f` the role token, masked — the key line is drawn at 80×24, above the prose that may fall below the fold |
| after a removal | the removed agent's key, marked `(no agent with that name is configured; left by a credentials file last written <when>)`, and `D delete the leftover secrets of agents no longer configured` on this screen's own legend beside `k`/`f`/`A` | know that a listed secret belongs to no configured agent, **and how old the file that holds it is**, so a tidy-up is not a loss (C-4); delete it: `D` opens a confirmation naming the agents and removes exactly those entries, never showing a value; the key is offered where the leftover is shown, not only on a help page |
| a secret is missing | `present  nothing yet — no model key and no Forgejo token`; a refusal elsewhere names this screen's own keys (`press 5 and then k on the Credentials screen`), never a file and a TOML section | a key on this screen to supply it (`k`, `f`, `A`), never a file to edit |
| not ready | `the credentials file is not ready: <reason>` | the reason, and that no value is ever printed. A file that cannot be read is **not** reported as empty |

## Runs (`6`) — recent runs

| State | Shows | Must offer |
|---|---|---|
| empty | “No run has been recorded yet…” | where records will appear, and how a run is started |
| a run in progress | `running (<elapsed>)` (not `unknown start`); the transcript carries a `model_request_sent` written **before** each call, and the log says `waiting for the model`, so a wait is not a wedge | the start time from the run's own opening transcript record (not `plan.json`, which carries none), and that the run has not ended |
| finished | outcome with its reason and duration; the reason leads the note area, and the note area grows to hold it, so it is never the line that wraps off — but never below the list's own minimum of border, header and **three** runs | `o` the log, `T` the **transcript**, both named at 80×24 even beside a wordy reason; the pager's `q` named; a run left by a dead UI reports the time it actually ran, never `0.0s`; the paths sit below the offers and are reachable with `[`/`]` |
| many runs | newest first; a window whose title reads `recent runs a–b of N — ↓/↑ more`; the selected row carries `>`; the `started (UTC)` column says which clock it is; a run id too long for its column is shortened with an ellipsis, never clipped mid-token, and the outcome column keeps room for `succeeded, 0.0s` even at the 72×18 floor — taking it from the run-id column, which is already shortened with a marker — so no cell ends mid-value (T-3); the list holds the newest fifty records and, when more exist on disk, the note area says how many older records are not listed — naming the configured agents' records, the unlisted agents' records and the total on disk as distinct quantities, so no two different `on disk` numbers sit on one screen (L-1, F4, H-4) | `enter` opens the run's agent; selection readable in text, and never on a hidden row |
| records on disk | `N run records on disk, <size> of file bytes` under the record path; when some belong to an agent the configuration no longer lists, how many; and that the product never deletes a record, so a person removes a run's directory to free its space; the size is the sum of **file bytes**, the same definition every size on every screen uses, and it is labelled in the binary units the arithmetic uses (`KiB`, `MiB`, `GiB`, and `B` below 1024), never `KB` over a value divided by 1024 (H-4, F-2) | the footprint and the cleanup expectation, on the screen that lists the records: where they live, how much room they take, and what is expected of a person (L-2, L-4) |

## Overlays

| Overlay | State | Must offer |
|---|---|---|
| first-run acknowledgment | shown once per configuration directory | own its rows (no bleed from the panel); `y`/`n`; the record path **at the advertised minimum too** (80×24), never a sentence that ends without its object; `R` on any screen shows the same terms again **read-only** when a record exists (the footer reads `esc closes this overlay`, and no key changes the record) and the ordinary `y`/`n` modal when none does (C-3) |
| any overlay over a screen | the panel and the overlay | the footer's status rows stay visible under the overlay — including the help overlay, which used to cover them — so a sentence about what just happened (a paste kept only its first line, a refusal, or the help's own "press PageDown" note) is never cleared away; and the footer's key legend never advertises `q quit` while an overlay owns the keys (there `q` is a letter in a field or a cancel), reading `esc closes this overlay` instead. A **paste** under an overlay is not dropped in silence either: the status says what owns the screen and how to reach a field (F4) |
| add-agent form (`n`/`a`) | opened from the Agents screen | name, endpoint, model, key, Forgejo URL, **role** token — every field drawn at 80×24, with the body windowed so the cursor's own row is always drawn; writes the two files; the model key is **required**, so the agent the form creates can run; the form says the agent's own token is separate and is supplied later with `v`; the keys never echoed; **the `enter writes it / esc cancels` action line is in the panel at every size** (including 80×24); a masked field shows how many characters it holds; a bracketed paste keeps the first line, names the rest as unused, and **replaces** the field instead of appending |
| secret prompt (`k`/`f`/`v`/`A`) | Credentials (`k`, `f`, `A`) and Forgejo (`v`, `A`) | one field, masked, showing how many characters it holds (`1 character` in the singular); writes the named secret to the credentials file, naming the agent it belongs to when the secret is per-agent; never a value on screen; a bracketed paste keeps the first line and replaces |
| removal confirmation (`d`) | any configured agent | `y` or `enter` removes it from the configuration, `n`, `esc` or `q` keeps it — the destructive key is named in the box, and `q` is never the destructive one; the files left on disk are named **only when they exist**, and the run records under the state directory and the credentials file are named too, with where to delete the secret |
| leftover-secret delete (`D`) | Credentials, when a secret's agent is no longer configured | the agents are named (never a value); `y`/`enter` deletes exactly those entries from the credentials file and says what was deleted; `n`, `esc` or `q` keeps them |
| key help (`?`/`h`/`F1`) | any screen | every key, in groups, with one line of what it does; a **scrollable window** whose title reads `keys a–b of N — ↓/↑ more` when the window is short, so every key survives 80×24; it scrolls with up/down (and `j`/`k`), a page with PageDown/PageUp, and Home/`g` or End/`G` to either end; `[`/`]` scroll a panel or note area and `D` deletes a leftover secret; a key it cannot use says so instead of being swallowed |

## Hand-offs (editor, pager)

| Hand-off | State | Must offer |
|---|---|---|
| editor, `$EDITOR` set | the user's own editor | a confirmation naming the program before it opens, with `enter` to open it and `esc` to cancel — the same confirmation the unset case uses, because a status line is erased the moment the UI steps aside for the editor (N-1) |
| editor, `$EDITOR` unset | `vi` (or `nano` when present) | the program named, and how to leave it — the overlay gives `vi`'s `:wq`/`:q!` and `nano`'s `^X`, and the `nano` advice is true whether or not the editor asks to save (`^X leaves; if it asks, Y and enter saves and N does not`), because `nano` with an unmodified buffer leaves at once and a `N` typed afterwards belongs to the product (F5); **the hand-over is visible in the after direction too**: every edit and view outcome ends by saying the keys are the product's again, so the return is never silent (T-2); the sentence's verb agrees with its subject (`the declared steps … open in`, `the instruction … opens in`) (F6); an honest “not changed” when nothing was saved, and an honest “created” when the hand-off itself made the file the screen beside it now counts, with **the file's own name** in every hand-off status (`steps.txt`, `SKILL.md`, `instruction.md`) (H-1, H-2, W-4); backing out of the confirmation creates nothing |
| editor on skills (`K`) | a scratch `SKILL.md`, not a directory | what a skill is and the file name; the file is written only when the editor opens, and an untouched template is reported as created, with the consequence named (a run installs every skill under `skills/`) (H-1) |
| editor on an instruction | a file whose only guidance is one marked placeholder line | **the product's own placeholder text** is dropped from the document's first content line before a run sends the document — including when the person typed into the line the editor opens on, so the guidance is never sent glued to their sentence — while a copy of the placeholder later in the person's own document is kept, and the user's words around it are not deleted (T-1); an emergency save left by an interrupted editor is named before the hand-off |
| pager (`o`, `T`) | the user's own `$PAGER` | the path shown before it opens, and how to leave it (the pager's own `q`), because `esc` is this product's way back but not the pager's — and `esc` there can begin a sequence that swallows the next key, so the sentence names that trap and says to press `q` again; reading leaves the person's `HOME` as it found it (H-6) |
| the state a hand-off program keeps | each program the hand-off spawns | a state location of its own for every program the product chooses: `less` a history file at `/dev/null`, `nano` an `$XDG_DATA_HOME` in a scratch directory removed when it exits (so no `~/.local/share/nano`), `vi`/`vim`/`nvim`/`ex` `-i NONE` (so no `~/.viminfo`); a program the person chose with `$EDITOR`/`$VISUAL`/`$PAGER` that the product does not know keeps its own files, and is named in the hand-off sentence before it takes the screen — the product does not pretend to know what it writes (F7) |

## Signals and abandoned runs

| State | Shows | Must offer |
|---|---|---|
| the UI is signalled | — | the runs it started in `tui` mode end with it: the child asks the kernel to end when its parent does |
| the UI is left with `q` while a run is in flight | one warning naming how many runs will stop (`1 run`, not `1 run(s)`), with the pronoun in the same number (`stop it` / `stop them`) | press `q` again to leave and stop them, or any other key to stay; staying **replaces the warning** with a plain statement rather than re-asking the question, and the staying key is **consumed**, so an unbound key cannot erase the statement with `no key is bound to that`; the record of each ended run names the UI closing, not “on request” |
| the UI is reopened after a run was left behind | the agent as `stopped`, never `idle`; the run's duration is the time it actually ran | the run's own record is closed as `stopped` with a reason, so a second run of the same agent never overlaps an abandoned one, in the same workspace |
| the UI is reopened after a run finished | the agent's newest terminal outcome, its reason and its elapsed time | the hub seeds each agent from its newest terminal record, so a restart never shows `idle` for an agent whose record says `succeeded`, `stopped` or `failed` |
| the configuration is re-read (`r`) after an agent was removed by hand | the removed agent's run is stopped, and the note names it once, in the note area | the run does not stay counted but invisible; what the footer counts is what a screen can show; the run's own record names the re-read, never “on request” |
| the configuration is re-read (`r`) after the run records were deleted | the hub drops an outcome whose record is gone; the hub and the ledger agree; when a cached outcome is dropped the hub says a record it had is no longer there, and an older record left is not called “the last run” | a cached outcome never outlives the record it came from, and a person whose history was edited by hand is told |
| a start fails before a run ever runs (systemd refuses, the process cannot spawn) | the run state is `failed` with the start's reason, not `unknown start / no terminal record` | the record opened for the start is closed as `failed`, so the next `s` is not blocked by a run that never was |
| `^C` | — | a control chord is never a plain-letter binding: `^C` asks the same question `q` asks when runs are in flight (naming the runs and the way to stay), and a second `^C` leaves and stops them; with nothing running it leaves at once. Inside a hand-off (editor, pager) `^C` belongs to the user's own program: the product catches the signal for itself, so the hand-off's own exit returns to the product instead of killing it and stopping the runs, and when it regains the terminal it asks the same leave question |

## Every screen — footer

| State | Shows | Must offer |
|---|---|---|
| wide terminal | the status line and the key legend | the legend may be long |
| every screen, every state | a footer clause `Running: N of M`, and `· K finished` once a run has ended since this UI started; when the newest fifty records all finished since the UI started and older records exist on disk, `· K+ finished`, so the cap is never presented as exact (L-5) | the running count and what has finished, wherever the user is, not only on the hub |
| 80×24 | the status line and the legend | the legend must not silently drop keys: it clips at a key boundary and ends with `? all keys`, and the way back (`esc agents`) is among the shown keys |
| any status longer than the width | the status line, grown past its usual rows when it must be | the footer takes the body's spare rows for the status (never below the body's minimum), so a reason, a path or git's own `fatal:` line is read whole at 80×24, never clipped |
| a window smaller than 72×18 | one line: “this window is too small to show the product; make it at least 72×18” | that line, instead of clipped panels, a clipped modal or a clipped help overlay |
| exactly 72×18 (the advertised minimum) | the product, with at least one agent row drawn and the count in the title when more exist, and the tab bar with all six names whole (`6 Runs`, not `6 Ru`) | a minimum the product calls usable must show a real row, not a header over nothing, and must name every screen it offers |
