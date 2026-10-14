# gewu-multiverse v0.1.5 — the fifth fix round

This is the sixth prebuilt release of `gewu-multiverse`. It is built from `main`
after the fifth round of interface fixes, and packed so you can try it without a
Rust toolchain. Nothing about the install or the design changed: if `v0.1.4` ran on
your machine, this one runs there too. The binary reports the version it is named
for: `gewu-multiverse --version` prints `0.1.5`.

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

If you are upgrading from `v0.1.2`, this build carries **three** rounds of fixes:
the third fix round, which `v0.1.3` published; the fourth fix round, which `v0.1.4`
published; and the fifth fix round, which is this build. If you are already on
`v0.1.4`, the new work is the fifth round, listed first; the earlier rounds are
summarised after it, so a `v0.1.2` reader loses nothing.

### The fifth fix round — new in this build

The fourth round fixed what *watching the same product under a paste, a closed
window, and a Forgejo list longer than its panel* revealed. This round fixed what
*watching someone try to leave a mode, read a cause that had been cut off, and trust
a screen that was not telling the truth* revealed. Each fix has a test behind it, and
the classes were swept: every path of the class is listed as fixed, already correct,
or open with a reason.

- **Switching supervision is never a one-way door.** Pressing `m` to move an agent
  into `systemd` now asks the machine first whether a user unit can exist there. On a
  launch with no reachable user manager — `su`, a plain `ssh`, a service context — the
  switch is refused **before** the mode record or a unit is written, and the refusal
  says that nothing changed, that the agent is still supervised by this UI, and what a
  systemd switch needs. Returning to `tui` no longer needs the manager when no unit
  file is on disk, and the Agent screen prints, beside the mode, a line naming an
  unreachable manager and the way back. Leaving the mode is possible even when the
  machine that would host it is gone.
- **A failed clone's own cause is not clipped.** The footer status area now grows to
  hold whatever it carries — word-wrapped, with long words broken — taking the body's
  spare rows down to the body's own minimum and no further. A clone failure is two
  logical lines (the account it acted as, then git's own `fatal:` line); at 80×24 both
  are drawn where the fixed three rows used to stop at the first. The same rule keeps
  a whole configuration diagnostic, its caret line and its `r` sentence on screen, and
  sits the recovery key beside the error instead of only in a legend a short window
  clips.
- **The Instruction screen names the real difference.** It used to call any gap
  between the size on disk and the size a run sends “the guidance line is dropped”. A
  file with no guidance line but a trailing newline differed by that byte alone, so
  the screen announced a line that never existed. It now says whether the guidance
  line was dropped, whether the trailing newline is not sent, or just the one size —
  according to the fact. Only the product's own first guidance line is dropped; a line
  of your own that matches it later is kept as your text.
- **The Credentials screen shows the mode the file actually has.** The mode was the
  value cached when the app started, so a file widened by hand printed `mode 0600`
  beside the refusal that said it was 0644. The mode is now read from disk each frame,
  so the two agree. A one-character secret reads `1 character`, not `1 characters`, and
  the model-key prompt names the agent whose key it is about to write.
- **A missing endpoint path is named, not the model blamed.** The sample and the
  add-agent form now show the full completions address, and a 404 adds that a base URL
  may need `/chat/completions`, so the product's own example no longer produces a
  failure attributed to the model. Before the identity key `i` creates or checks an
  account on your Forgejo, the line names the account it would settle
  (`gewu-<agent>`); the role-check prompt no longer begins with a stray capitalised
  word.
- **Honest records for starts, stops and re-reads.** A start that opened a run
  directory and then failed is closed as `failed` instead of sitting as
  `unknown start` for ever and blocking the next start; a run stopped because a
  configuration re-read no longer listed its agent records that, rather than “on
  request”; `r` drops an outcome whose record has been deleted, so the hub and the
  ledger agree; an unknown field reads “not a valid configuration” rather than “not
  valid TOML”; the quit warning becomes a plain statement once you choose to stay;
  and counts use one/other.
- **A panel that hid fields says so.** At 80×24 the Agent screen dropped fields below
  the fold with nothing indicating more existed and cut a path mid-filename; its title
  now says `more fields below; enlarge the window` and paths wrap. The Forgejo role
  prose is shortened once checked, so the checked repository list gets the panel.
- **The release-lag finding is process, not product, and is answered.** The fourth
  round was on `main` but in no release for a while; nothing in the binary was wrong —
  the loop published too late. The answer is the rule *a release follows every fix
  round*, and this is that release. No version-check feature, no banner and no
  self-updating binary was added: the product does not phone home.

### The fourth fix round — carried from v0.1.4

The Forgejo repository list no longer hides a repository (a window titled
`repositories a–b of N — ↓/↑ more`, the selected row always drawn and marked `>`);
removing an agent while it runs is refused with the run named; quitting with a run in
flight warns once and records that the UI was closed; a second start speaks in the
operator's voice; the hub and the Runs ledger use one word for success; a bracketed
paste into a one-line field keeps one line; the add-agent form's action line stays
inside the panel and the key help is a scrollable window; the Instruction screen shows
the size on disk and the size a run sends; a failed clone names the account it acted
as and the list's source before git's detail; modals leave the footer's status rows
visible; and the smaller truths each carry a test.

### The third fix round — carried from v0.1.3

The agent list no longer hides an agent (a window titled `agents a–b of N — ↓/↑ more`,
the selected row always drawn and marked `>`); a restart seeds each agent from its
newest terminal record, so the hub shows the last outcome and reason rather than
`idle`; the footer carries `Running: N of M` on every screen and `· K finished` once a
run has ended since the window opened; a signalled stop reads `stopped … is proven
gone` once the record closes; the removal confirmation only promises files that exist;
the Forgejo screen shows the identity and its mode persistently; the Runs note area
leads with the reason; an interrupted editor's emergency save is named; a credential
belonging to a removed agent is marked as such; and one size below the advertised
minimum says the window is too small instead of clipping in silence.

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
- **The crate version string is `0.1.5`, and it matches the tag.** `Cargo.toml` and the
  committed `Cargo.lock` both carry it, so `gewu-multiverse --version` reports the version
  the release is named for, and `cargo build --locked` means something on a fresh clone.

## How it was built

`cargo build --release --locked` on x86_64 Linux, with rustc 1.98.1 and cargo 1.98.1,
target `x86_64-unknown-linux-gnu`, at revision
`ccb2a8be9dbe82e6883eec2bea3a05c533c40c69`. The repository commits its `Cargo.lock`,
so the build held `--locked` against the committed lock. The tarball also holds the
README and both licence files.

## Checksum

```text
893a2c25b7d249cef22bfa1fc5af3cd5e4e84df5ceb4ca5af2c2275c33b834c2  gewu-multiverse-ccb2a8be9dbe-x86_64-linux.tar.gz
```

The tarball is 2,246,768 bytes. Verify it with
`sha256sum -c gewu-multiverse-ccb2a8be9dbe-x86_64-linux.tar.gz.sha256`.
