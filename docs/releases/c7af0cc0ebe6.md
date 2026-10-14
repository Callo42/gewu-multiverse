# gewu-multiverse v0.1.8 — the eighth fix round

This is the ninth prebuilt release of `gewu-multiverse`. It is built from `main`
after the eighth round of interface fixes, and packed so you can try it without a
Rust toolchain. Nothing about the install or the design changed: if `v0.1.7` ran on
your machine, this one runs there too. The binary reports the version it is named
for: `gewu-multiverse --version` prints `0.1.8`.

## What it is

`gewu-multiverse` is a local, single-Universe Matrixverse — one small terminal
application that turns a Linux machine into a place where you run **agents against
your own Forgejo**, with no server, no accounts and no cloud. You install it, open
the terminal UI, point it at one Forgejo, and define each agent with its own model
endpoint, key and instructions. Agents run as children of the UI by default, or as
user units that outlive the window when you ask for that; every run leaves a log and
an append-only JSONL transcript. The README describes all of it, and the
[quickstart](https://github.com/Callo42/gewu-multiverse#quickstart-from-nothing-to-a-running-agent)
walks from install to a run's log.

## What changed since v0.1.2

If you are upgrading from `v0.1.2`, this build carries **six** rounds of fixes: the
third fix round, which `v0.1.3` published; the fourth, which `v0.1.4` published; the
fifth, which `v0.1.5` published; the sixth, which `v0.1.6` published; the seventh,
which `v0.1.7` published; and the eighth fix round, which is this build. If you are
already on `v0.1.7`, the new work is the eighth round, listed first; the earlier
rounds are summarised after it, so a `v0.1.2` reader loses nothing.

### The eighth fix round — new in this build

The seventh round made every panel and note area scroll, and gave a scrolled surface
its `↑ more above` marker. This round fixed what *reading that answer on the machine
the Auditor actually installed it on* revealed: the scroll offset was shared across
screens, the marker was computed only for the unscrolled state, Forgejo below the
advertised floor declared content it could not reach, a missing key was refused by
naming a file and a TOML section instead of the product's own key, the tab bar
clipped its last name at the floor, and the Credentials legend did not offer the
delete it showed the need for. Each fix has a test behind it, and the classes were
swept: the offset, the refusal wording, the below-floor content, the tab bar and the
"a legend offers the key for an object on screen" rule were each audited per path.

- **The scroll offset belongs to the screen it was made on.** Every way of changing
  screens — the number keys `1`–`6`, `tab` and `left`/`right`, `esc`, and `enter` into
  an agent — now returns the new screen to its own top, so a screen the user never
  scrolled never opens at another screen's offset. The three paths that set the screen
  directly were the hole; every screen change now goes through one function that
  resets the offset when the screen changes.
- **A scrolled surface never hides its own top in silence.** A bordered title grows the
  sibling marker (`↑ more above`, or `↑ more above — more below; enlarge the window`
  when both apply), and a scrolled note area opens its drawn rows with
  `↑ more above — press [ to go back`. The markers are true at every offset, not only at
  offset zero.
- **Below the advertised floor, Forgejo scrolls.** At 80×18 and 72×18 the repository
  list and the key legend were declared but unreachable, the selection was invisible,
  and `[`/`]` refused by naming a panel that was not on the screen. `[`/`]` now scroll
  the Forgejo panel — summary, `repositories 1–N of N`, every repository with its `>`
  marker, the identity report, the clone note and the two-row legend — so the below-fold
  content is reached rather than merely declared, and the refusal never names a panel
  that is not there. At 80×24 the windowed form is unchanged, so the legend still stays
  pinned where it fits.
- **A refusal names the product's own key, not a file.** A start with no model key said
  "add one under `[model_keys]` in credentials.toml"; it now says
  `press 5 and then k on the Credentials screen to add one`. The sibling refusals were
  swept: a clone with no recorded identity names `v` on the Forgejo screen; the `i`
  identity errors name `v`/`A` and the screens they are on, never
  `[forgejo_agent_tokens]` or `forgejo_admin_token`; and the `P`/`U` token reads name
  `v`/`i` when the token is missing while keeping the file's own sentence for any other
  read failure. The run's own log and transcript are deliberately unchanged — that is
  the audit record, and it keeps the file's exact sentence.
- **The tab bar keeps every name at the advertised floor.** At 72 columns the six names
  plus the bar's side padding came to one cell more than the inner width, so `6 Runs`
  was clipped to `6 Ru`. The padding is gone (the divider already separates the names):
  the bar reads `1 Agents 2 Agent 3 Instruction 4 Forgejo 5 Credentials 6 Runs` whole at
  72 and wider.
- **The legend offers the delete it shows the need for.** With a leftover secret present
  (`model key for \`ghost\` (no agent with that name is configured)`) the Credentials
  screen's own legend now gains
  `D delete the leftover secrets of agents no longer configured`, directly under the
  `k`/`f`/`A` row; `D` was previously on page three of the help. With no leftover, the
  legend is unchanged.

### The seventh fix round — carried from v0.1.7

Every panel and note area scrolls, and no key is swallowed: `[` and `]` move a panel's
own body on every screen, the arrows and page keys move the panels whose content is not
a list, and a press that cannot move it says `the panel is at its top` / `at its end`.
The overflow declaration is never the part that is clipped (the title is composed from
its parts and the range moves above the list only if nothing holds both). The Runs note
does not squeeze the run list to one row. A record the hub had that is gone is named,
and an older failure is not promoted to "the last run". The endpoint test is dated and
goes stale visibly. Removal names every place it leaves files, and a leftover secret can
be deleted. A missing log is marked, and the Forgejo legend keeps each phrase whole.

### The sixth fix round — carried from v0.1.6

Nothing truncates in silence: every bordered panel puts `more below; enlarge the
window` in its title when its content exceeds the window, and the two borderless note
areas reserve their last drawn row for the same sentence; each screen's own must-offer
sentence is ordered above prose that can wait. The hub answers for the last run, not
the last failure. A transport failure is on screen in the product's words (`the
endpoint refused the connection`, `the connection dropped`, `the address could not be
resolved`) instead of the library's `io:` / `os error N`, while the run's own log keeps
the exact sentence. The editor and pager hand-offs name their own way out. The smaller
truths: counts read as sentences, a run id too long for its column is ellipsised,
PageDown/PageUp and Home/End move the selection, the tab bar carries each screen's
number, and `esc agents` is among the keys the legend draws.

### The fifth fix round — carried from v0.1.5

Switching supervision is never a one-way door: `m` asks the machine first whether a
user unit can exist there, refuses before writing anything when no manager is
reachable, and leaving `systemd` needs no manager when no unit is on disk. A failed
clone's own `fatal:` cause is no longer clipped. The Instruction screen names the real
difference (a dropped guidance line, or just an untrailed newline). The Credentials
screen shows the mode the file actually has, read each frame. A missing endpoint path
is named rather than the model blamed. A panel that hid fields says so.

### The fourth fix round — carried from v0.1.4

The Forgejo repository list no longer hides a repository (a window titled
`repositories a–b of N — ↓/↑ more`, the selected row always drawn and marked `>`);
removing an agent while it runs is refused with the run named; quitting with a run in
flight warns once; a bracketed paste into a one-line field keeps one line; the
add-agent form's action line stays inside the panel; a failed clone names the account
it acted as before git's detail; and the smaller truths each carry a test.

### The third fix round — carried from v0.1.3

The agent list no longer hides an agent (a window titled `agents a–b of N — ↓/↑ more`,
the selected row always drawn and marked `>`); a restart seeds each agent from its
newest terminal record; the footer carries `Running: N of M` on every screen; a
signalled stop reads `stopped … is proven gone` once the record closes; the removal
confirmation only promises files that exist; the Forgejo screen shows the identity and
its mode persistently; a credential belonging to a removed agent is marked as such; and
one size below the advertised minimum says the window is too small instead of clipping
in silence.

## What it needs

- A 64-bit **x86_64** Linux machine with **glibc 2.39 or newer** — Ubuntu 24.04 or newer,
  Debian 13 or newer, Fedora 40 or newer, or any distribution that new. The binary is
  dynamically linked against your system's glibc; it is not a static build. The floor was
  read from the ELF (`.gnu.version_r` names `GLIBC_2.39`), not assumed.
- A **terminal**. The UI needs a terminal on standard input and standard output; started
  without one it stops with a sentence rather than a panic.
- **Your own `git`**, when you ask an agent to work on a repository.
- At **first run**, nothing beyond the above: the tool writes a commented `config.toml`
  and a `credentials.toml` (mode 0600) under `~/.config/gewu-multiverse`, and asks you
  once to acknowledge that a run acts with your rights and is not a sandbox. Press `y`
  to record it, or `n` to leave it; a start stays refused until it is recorded.

## What it deliberately does not do yet

- **No build for other machines.** This is x86_64 Linux only: there is no aarch64 build,
  no musl/static build, and no macOS or Windows build. On a machine with an older glibc
  than 2.39, use the source install instead.
- **No package format.** The release is a tarball, a checksum and this note: there is no
  `.deb`, no `.rpm`, no container image. The repository's `install.sh` performs the
  download, checksum and install in one line.
- **No service.** The terminal UI is the interface. An agent can be put under your own
  user systemd from the UI — that is documented — but nothing is installed as a service.
- **No sandbox.** An agent runs as you. That is a layout, not a security boundary, and
  the README says so at length.
- **The crate version string is `0.1.8`, and it matches the tag.** `Cargo.toml` and the
  committed `Cargo.lock` both carry it, so `gewu-multiverse --version` reports the version
  the release is named for, and `cargo build --locked` means something on a fresh clone.

## How it was built

`cargo build --release --locked` on x86_64 Linux, with rustc 1.98.1 and cargo 1.98.1,
target `x86_64-unknown-linux-gnu`, at revision
`c7af0cc0ebe6a9bf194935cf8e9c8de0f8e85259` (the version commit on the release branch).
The repository commits its `Cargo.lock`, so the build held `--locked` against the
committed lock. The tarball also holds the README and both licence files.

## Checksum

```text
32d0d93816d3ec55ab347eeb2d29e551e8087fabc4771affa3b4060c697c4708  gewu-multiverse-c7af0cc0ebe6-x86_64-linux.tar.gz
```

The tarball is 2,269,780 bytes. Verify it with
`sha256sum -c gewu-multiverse-c7af0cc0ebe6-x86_64-linux.tar.gz.sha256`.
