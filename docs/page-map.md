# The page map

Every screen the product can show, and every state a user can be in on it: what the
screen shows, and **what it must offer a zero-knowledge user at that moment**. The map
is the standard the screens are built and fixed against; a screen that hides the next
step, or shows a word the user has not been given, is a defect against its row.

A zero-knowledge user has started the product and nothing else. They have not read this
repository, the README or any page. The product carries them: each state says what it
is, what is missing, and the key or the form that moves them on.

The product's vocabulary is small and fixed: **agent**, **endpoint**, **model**,
**instruction**, **skill**, **steps**, **run**, **role**, **identity**, **credentials
file**, **configuration file**, **supervision**, **concurrency**, **max turns**. A
screen may use a word only after the screen, or a screen before it, has shown what it
means. `concurrency` (how many model requests one agent may keep in flight at once)
and `max turns` (the most model turns one run may take) are defined in the Agents note
area where they first appear.

Screens: **Agents** (`1`) · **Agent** (`2`) · **Instruction** (`3`) · **Forgejo**
(`4`) · **Credentials** (`5`) · **Runs** (`6`). Overlays: **first-run
acknowledgment**, **add-agent form**, **key help**. Hand-offs: **editor**, **pager**.

---

## Installer output (before any screen)

| State | Shows | Must offer |
|---|---|---|
| installed, install directory on `PATH` | `installed <path> (<tag>)`, `run it with: gewu-multiverse` | a closing line that resolves when typed |
| installed, install directory **not** on `PATH` | `installed …`, a `note` about `PATH`, `run it with: gewu-multiverse` | the closing line must be the command that works: the absolute path |

## Agents (`1`) — the hub

| State | Shows | Must offer |
|---|---|---|
| first run, acknowledgment not recorded | the risk overlay (see below) over the Agents screen | one legible overlay; `y`/`n` both act; `n` leaves the list visible |
| nothing configured | empty list; no agent; a note naming the configuration file | a form, by name: “press `n` to add an agent”. `n`/`a` must answer |
| partially configured | one or more rows: name, model, concurrency, supervision, state; the selected row carries `>` | each row readable **and present**; the last failure named, not only the word `failed`; `concurrency` and `max turns` defined in the note area |
| ready (idle) | rows with `idle`; the concurrency count and the note about `tui` | the keys that matter (`p`, `s`, `c`, `e`, `d`) visible or one `?` away |
| many agents (more than fit) | a window of rows whose title reads `agents a–b of N — ↓/↑ more`; the selected row is always drawn and marked `>` | no agent is hidden **silently**, and the selection never moves onto a row that is not drawn |
| removing | a confirmation over the list | `y` removes the agent from `config.toml`; `n` keeps it; the files left on disk are named, or the confirmation says it had none yet |
| running | `running (Ns)`; `Running: 1 of N allowed` | the run id and the stop key named; the Runs screen agrees |
| waiting for a slot | `waiting (1 in line)`; “is waiting its turn: …” | the slot count and the line position; no error |
| stopped by `x` | `stopped`; “its process group was signalled and is proven gone” once the record is closed | the stop is confirmed, in the past tense, so the status and the row agree; the agent can be started again |
| failed | `failed (class)`; the last reason in the note area | **why**, on this screen: the class and the run's own reason |
| error (config unreadable) | `no agent list: <reason>` | the reason in the product's words and a way back (`r`, or the form) |

## Agent (`2`) — one agent's configuration

| State | Shows | Must offer |
|---|---|---|
| no agent | `no agent to show: …` | the same in-app path as the empty Agents screen |
| configured | name, model, endpoint, max turns, instruction, **concurrency**, run state, supervision, unit/restarts, agent files, workspace, newest log, endpoint test, identity | every word the user needs defined here or on the screen that introduced it; no `identitynot` |
| removing | the same fields, with the removal confirmation over them | `d` removes the agent (after asking); the files left on disk are named, so a mistyped agent is not a dead end |
| failed | run state `failed`; the failure class and the run's reason | the reason stored, not a transient footer line |
| systemd, unit start failed | run state; unit; restarts | the `systemctl` reason, recoverable |
| systemd, ready | supervision `systemd (user unit)`; unit name; restarts | what `systemd` changes *before* `m` is pressed |

## Instruction, steps and skills (`3`)

| State | Shows | Must offer |
|---|---|---|
| no agent | `no agent to show` + the config path | the in-app path to add one |
| empty instruction | path, `not there yet — press e to write it`; a file that holds only the guidance placeholder reads the same way, with the reason | `e`, and one line of what a run sends |
| instruction written | path, size, steps, `skills N on disk` | a preview of the instruction, or a key that opens it |
| an interrupted edit | `an earlier edit of this instruction was not saved; the editor kept it in <file>` | the emergency-save file named, so the words are recovered, not retyped |
| steps | path, declared count, or `none — press t` | the template must not doubt the product (“until model calls exist” is false) |
| skills | `skills N on disk`, one `SKILL.md` per skill | what a *skill* is, and `K` must not hand an editor a directory |

## Forgejo (`4`) — connection and role check

| State | Shows | Must offer |
|---|---|---|
| no token | `role token absent`; `admin token absent (identities are supplied)`; “no role check has been run yet — press c” | what a *role* is, in one line, before `c` is pressed |
| configured, unread | the URL and role | a sentence that says *not read yet*, not “could not be read” |
| checked (one token) | the view, the repositories, each `[private]`/`[public]`, what the caller may do | which repository is selected, in text (a `>` marker), not only in colour |
| checked rows then `g` | the clone acts as the agent's identity, while the list came from the role token | a failed clone names the account it acted as and says the list came from the role token, so two different tokens are not confused |
| both tokens present | one view at a time, or both | a way to compare the two roles **inside the app** (`c` / `C`); a failed `C` keeps the last good view and shows the failure beside it |
| a token is missing | `role token absent` / `admin token absent` / `failed (no_agent_token)` | a one-field, masked prompt (`v` for the agent's own token, `A` for the administrator token) that writes the credentials file, never an editor |
| `not_admin` | the exact refusal | keep it; it is the model |
| identity not settled | `identity not settled — press i` | what `i` will change, before it creates an account, and the `v` prompt that supplies the token `i` needs |
| identity settled | `identity gewu-<agent> (managed|supplied) — clones and pull requests act as this account` | the login **and** the mode persistently, not only in a transient status line |

## Credentials (`5`)

| State | Shows | Must offer |
|---|---|---|
| ready | file path, `mode 0600`, the redaction promise | which secret **names** are present (`model key for…`, `Forgejo role token`), never a value; `k` sets a model key and `f` the role token, masked |
| after a removal | the removed agent's key, marked `(no agent with that name is configured)` | know that a listed secret belongs to no configured agent, without deleting it silently |
| a secret is missing | `present  nothing yet — no model key and no Forgejo token` | a key on this screen to supply it (`k`, `f`, `A`), never a file to edit |
| not ready | `the credentials file is not ready: <reason>` | the reason, and that no value is ever printed. A file that cannot be read is **not** reported as empty |

## Runs (`6`) — recent runs

| State | Shows | Must offer |
|---|---|---|
| empty | “No run has been recorded yet…” | where records will appear, and how a run is started |
| a run in progress | `running (<elapsed>)` (not `unknown start`) | the start time from `plan.json`, and that the run has not ended |
| finished | outcome with its reason and duration; the reason leads the note area, so it is never the line that wraps off | `o` the log, `T` the **transcript**, both named |
| many runs | newest first; a window whose title reads `recent runs a–b of N — ↓/↑ more`; the selected row carries `>` | `enter` opens the run's agent; selection readable in text, and never on a hidden row |

## Overlays

| Overlay | State | Must offer |
|---|---|---|
| first-run acknowledgment | shown once per configuration directory | own its rows (no bleed from the panel); `y`/`n`; the record path |
| add-agent form (`n`/`a`) | opened from the Agents screen | name, endpoint, model, key, Forgejo URL, **role** token; writes the two files; the model key is **required**, so the agent the form creates can run; the form says the agent's own token is separate and is supplied later with `v`; the keys never echoed |
| secret prompt (`k`/`f`/`v`/`A`) | Credentials (`k`, `f`, `A`) and Forgejo (`v`, `A`) | one field, masked; writes the named secret to the credentials file; never a value on screen |
| removal confirmation (`d`) | any configured agent | `y` removes it from the configuration, `n` keeps it; the files left on disk are named **only when they exist** |
| key help (`?`/`h`/`F1`) | any screen | every key, in groups, with one line of what it does |

## Hand-offs (editor, pager)

| Hand-off | State | Must offer |
|---|---|---|
| editor, `$EDITOR` set | the user's own editor | a status line naming the program before it opens |
| editor, `$EDITOR` unset | `vi` (or `nano` when present) | the program named, and how to leave; an honest “not changed” when nothing was saved |
| editor on skills (`K`) | a scratch `SKILL.md`, not a directory | what a skill is and the file name |
| editor on an instruction | a file whose only guidance is one marked placeholder line | the placeholder is dropped before a run sends the document, so guidance is never sent as instruction; an emergency save left by an interrupted editor is named before the hand-off |
| pager (`o`, `T`) | the user's own `$PAGER` | the path shown before it opens |

## Signals and abandoned runs

| State | Shows | Must offer |
|---|---|---|
| the UI is signalled | — | the runs it started in `tui` mode end with it: the child asks the kernel to end when its parent does |
| the UI is reopened after a run was left behind | the agent as `stopped`, never `idle` | the run's own record is closed as `stopped` with a reason, so a second run of the same agent never overlaps an abandoned one, in the same workspace |
| the UI is reopened after a run finished | the agent's newest terminal outcome, its reason and its elapsed time | the hub seeds each agent from its newest terminal record, so a restart never shows `idle` for an agent whose record says `succeeded`, `stopped` or `failed` |
| `^C` | — | a control chord is never a plain-letter binding: `^C` leaves the UI and stops the runs it started |

## Every screen — footer

| State | Shows | Must offer |
|---|---|---|
| wide terminal | the status line and the key legend | the legend may be long |
| every screen, every state | a footer clause `Running: N of M`, and `· K finished` once a run has ended since this UI started | the running count and what has finished, wherever the user is, not only on the hub |
| 80×24 | the status line and the legend | the legend must not silently drop keys: it clips at a key boundary and ends with `? all keys` |
| any status longer than the width | the status line | wrap it onto the footer's spare lines, never clip a reason or a path |
| a window smaller than 72×18 | one line: “this window is too small to show the product; make it at least 72×18” | that line, instead of clipped panels, a clipped modal or a clipped help overlay |
| exactly 72×18 (the advertised minimum) | the product, with at least one agent row drawn and the count in the title when more exist | a minimum the product calls usable must show a real row, not a header over nothing |
