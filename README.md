# gewu-multiverse

**One line, no Rust:**

```sh
curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
```

It fetches the latest prebuilt release, verifies its checksum and installs `gewu-multiverse` into
`~/.local/bin` (x86_64 Linux, glibc 2.39+ — details and other options under [Install](#install)).
Then run `gewu-multiverse`.

A **local, single-Universe Matrixverse**: one small terminal application that turns a Linux machine into a place where
you run **agents against your own Forgejo** — no server, no accounts, no cloud.

You install it, open the terminal UI, and it lets you:

- point it at **one Forgejo** you already have (URL + token, and which role to act as — an admin token can see
  administrative views, an ordinary one cannot);
- define **how many agents** you want, each with its **own model endpoint, key and instructions**;
- use the agent identities and tokens so that an agent's commits are attributed to that agent, never to you;
- start, stop and watch agents, with a log and a transcript for every run — as children
  of the UI by default, or as **user units** that outlive the window, when you ask for
  that per agent;
- check the repositories you can reach — under the role you configured.

## What this deliberately is not

By design there are **no users, tenants or roles**, **no multiple GitLab instances**, **no provisioning of anything**,
**no complex agent lifecycle**, **no ATIF capture**, **no web interface or API server**, and **no cloud dependencies**.
The terminal UI and two files on disk — a configuration file and a credentials file — are the whole interface. Run
records are plain logs and JSONL transcripts.

## Status

**An agent runs, thinks, and works on one Forgejo.** In place now:

- the terminal UI, with a screen per thing you configure: the agent list and each
  agent's state, one agent's settings, its instruction and skills, the Forgejo
  connection and the role check, the credentials file, and the recent runs;
- one configuration file, validated on load, with a clear message instead of a crash
  when it is wrong — including the run timeout, the limit on agents running at once,
  and each agent's bound on model turns;
- one credentials file, created with mode **0600**, which holds the model keys and the
  Forgejo tokens. A secret is read only at the moment it is needed, and every
  transcript, log and error the tool writes has it redacted;
- the first-run risk acknowledgment, required once per installation before any agent
  may be started;
- **agents that actually run**: one child process per agent, in its own workspace, with
  an instruction, a skill set and a declared list of shell steps; started, stopped and
  watched from the UI, within a configurable concurrency limit and under a configurable
  timeout; each run leaving a log and an append-only JSONL transcript under
  `~/.local/state/gewu-multiverse/<agent>/<run-id>/`;
- **two ways to supervise an agent** (`m`): the default is a child of the terminal UI,
  which stops when the UI exits; the opt-in `systemd` mode runs an agent as a **user
  unit** of your own systemd, so it keeps running when the window closes, and is
  visible to `systemctl --user status`. The unit is written by the tool, holds no
  secret, runs exactly the command the UI would run, and restarts a failed run within a
  declared bound. See [Two ways an agent can be supervised](docs/systemd-mode.md);
- **a direct model call**: a run posts one non-streaming chat request to the endpoint
  its agent's configuration names, with the model id and the key from the credentials
  file. It runs the tool calls the model asks for inside the workspace, feeds the
  results back, and repeats until the model answers without a tool call or the declared
  turn bound is reached. Every exchange is in the transcript as it happened, with the
  key redacted;
- **one Forgejo, checked as the configured role** (`c`): the URL and the token are
  validated, and the repositories the token can see are listed with their namespace,
  whether each is private or public, and what the caller may do with it. An
  administrator's token shows an administrative view — the private repositories it does
  not own among them — and an ordinary token shows only what it may read; a bad URL, a
  bad token, or a token without the rights the configuration expects each fail with
  their own class;
- **one identity per agent, derived from configuration**: with an administrator token
  the tool creates and manages one Forgejo account per agent, named from the agent's
  own name, and stores its token in the credentials file; without one, you supply each
  agent's token and the tool records that this is the path in use;
- **repository work under the agent's identity** (`g`, `P`, `U`): the tool clones or
  opens a repository the role check listed into the agent's workspace and configures
  git there, so a commit made in that workspace is authored by the agent and not by
  you. It opens and updates pull requests as the agent through the Forgejo API. The
  agent uses the user's own `git`; no `glab` is needed;
- editing an agent's instruction, its skills and its declared steps from the UI, through
  your own `$EDITOR`, and reading a run's log through your own `$PAGER`.

The first-run/quickstart walkthrough is written down below, from install to a run's log and
transcript: see [Quickstart: from nothing to a running agent](#quickstart-from-nothing-to-a-running-agent).
See [Connecting a Forgejo](docs/forgejo.md), [How an agent runs](docs/agent-execution.md)
and [Two ways an agent can be supervised](docs/systemd-mode.md) for the details.

## Two ways an agent can be supervised

An agent runs as a **child of the terminal UI** unless you say otherwise. That is the
default, and it is what you want most of the time: the run belongs to the session you
started it in, and leaving the UI stops it and proves the process is gone.

When work should outlive the window it was started from, move an agent to the opt-in
**`systemd` mode** with `m`. The tool then writes one **user unit** under your own
systemd directory — `~/.config/systemd/user/gewu-agent-<name>.service`, never a system
unit, never root — and asks your own `systemctl --user` to start it. The unit runs
exactly the command the UI would run, with the agent's workspace as its working
directory, keeps no secret (the run reads the credentials file itself), and writes the
same log and transcript. Leaving the UI does not stop it; `x` stops it in either mode,
and `systemctl --user status gewu-agent-<name>.service` shows it like any other unit.
A failed run is restarted within a declared bound (`Restart=on-failure`, at most three
starts in sixty seconds), and every restart systemd reports is written into the run's
own log.

The two modes are two ways to supervise the same run, not two kinds of agent: the mode
is recorded per agent and can be switched at any time, **except while the agent is
running**, when the switch is refused with a sentence saying to stop it first.

This mode needs a **user session** (a systemd instance running as you), and
`loginctl enable-linger "$USER"` if an agent should keep running after you log out. It
is a convenience, **not a security boundary**: a unit runs as you, with your rights,
exactly like the default. [Two ways an agent can be supervised](docs/systemd-mode.md)
says all of this at length, including what the machine must have.

## Connecting a Forgejo

A Forgejo connection is one URL, one token, and — when you want the tool to make the
agents' accounts for you — one administrator's token. All of it lives in the two files
you already have; no source is edited and no secret is ever passed on a command line.

```toml
# config.toml — no secret here
[forgejo]
url = "https://forgejo.example.org"
# The role the token is expected to have. "admin" refuses an ordinary token;
# "member" accepts any account.
role = "member"
```

```toml
# credentials.toml — secrets only, mode 0600
# The token the role check uses: the repositories it lists are the ones this token
# can see.
forgejo_token = "…"
# Optional. With it, the tool creates and manages one Forgejo account and token per
# agent. Without it, supply each agent's token below.
forgejo_admin_token = "…"

[forgejo_agent_tokens]
# One token per agent name, used when no administrator token is configured:
# "scribe" = "…"
```

**The role check** (`c`) answers three questions: is the URL usable, is the token
accepted, and does the token have the rights `role` says it should. It then lists the
repositories the token can see — each with its `namespace/name`, whether it is
private or public, and whether the caller may read, write or administer it. The
difference between the two roles is visible in that list: an administrator's token
sees every repository, including private ones it does not own; an ordinary token sees
only the public ones and the private ones it has been given. A token that is not an
administrator's, under `role = "admin"`, is refused with `not_admin`.

**One identity per agent.** With `forgejo_admin_token` present, the tool creates the
account `gewu-<agent>` on the Forgejo, writes a mark on it so it is never mistaken for
a person, sets it a freshly generated password (used once, never stored), makes a token
for it, and writes that token under `[forgejo_agent_tokens]`. The credentials file
records the mode in use — `managed` for that path, `supplied` for the other — and the
login and email a commit will use. Without an administrator token, the tool reads the
token you put under `[forgejo_agent_tokens]`, asks the Forgejo whose it is, and records
that instead. Either way the result is the same: an agent whose commits are its own.

**Working on a repository.** After a role check, choose a repository on the Forgejo
screen and press `g`: it is cloned into the agent's workspace under the repository's
name, and the agent's git identity — the login and email recorded above — is written
into the clone's local configuration. The agent works with the user's own `git`, so a
commit made there is authored by the agent. `P` pushes the current branch and opens a
pull request as the agent; `U` updates the newest open one. A title and body can be
put in `<config dir>/agents/<agent>/pull-request.txt`, first line the title.

**Every request carries a token.** There are no anonymous calls. A token is read from
the credentials file and travels only in a request's `Authorization` header. Git is
handed the token through `GIT_ASKPASS`: git runs this same program with the prompt as
its argument, and the program answers from the credentials file over a pipe. The token
is never a URL, a command-line argument, a log line or a screen.


## An agent's model configuration

An agent's model is three things, and only one of them is a secret:

| Thing | Where it comes from | What it is |
|---|---|---|
| **endpoint** | the configuration file, `endpoint = "…"` | the URL the run posts its chat request to, called directly, with no relay in between. `http://` and `https://` are both accepted; a local endpoint is a normal thing to use. |
| **model** | the configuration file, `model = "…"` | the model id the request asks for. It is written into the request as `"model"`. |
| **key** | the credentials file, under `[model_keys]` (or one top-level `model_key`) | the bearer key. It travels only in the request's `Authorization` header. |

The configuration file is `~/.config/gewu-multiverse/config.toml`; the credentials file
is `credentials.toml` beside it, created with mode 0600. A key is looked up by the
agent's own name, so one installation can have a different key per agent:

```toml
# credentials.toml — secrets only, never printed, never logged
[model_keys]
"scribe" = "…"
```

The request shape is the widely used OpenAI-compatible chat-completions shape: a
`model`, a list of `messages`, and a list of `tools`. The exchange is deliberately
**not streamed**: one request, one reply, one transcript record. Each agent also has a
bound on how many turns one run may take (`max_turns`, 24 by default), and the run
timeout bounds it from the outside. Both bounds are visible in the run's record.

## Install

### Prebuilt binary (no Rust needed)

Each release carries a prebuilt **x86_64 Linux** tarball, so you can try the tool
without a compiler. It needs a 64-bit x86_64 Linux machine with **glibc 2.39 or newer**
(for example Ubuntu 24.04 or newer, Debian 13 or newer, Fedora 40 or newer) and a
terminal. Nothing else: no Rust toolchain, no build step, no service.

**One line** — this repository's [`install.sh`](install.sh) does the three steps below for you, looking up the
latest release and installing into `~/.local/bin` (override with `INSTALL_DIR`):

```sh
curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
```

**Or by hand** — the tarball holds the binary (`gewu-multiverse`), this README and both licences. Its
name records the revision it was built from:

```sh
# 1. download the tarball and its checksum (replace the tag if you use another release)
base=https://github.com/Callo42/gewu-multiverse/releases/download/v0.1.5
name=gewu-multiverse-ccb2a8be9dbe-x86_64-linux.tar.gz
curl -LO "$base/$name"
curl -LO "$base/$name.sha256"

# 2. verify the checksum
sha256sum -c "$name.sha256"

# 3. unpack and put the binary on your PATH
mkdir -p "$HOME/.local/bin"
tar xzf "$name"
install -m 0755 "${name%.tar.gz}/gewu-multiverse" "$HOME/.local/bin/"
```

If `~/.local/bin` is not on your `PATH` yet, add it:

```sh
export PATH="$HOME/.local/bin:$PATH"   # put this in your shell profile to keep it
```

Now run it — see [First run](#first-run) and the
[Quickstart](#quickstart-from-nothing-to-a-running-agent):

```sh
gewu-multiverse
```

### From source (needs Rust)

```sh
cargo install --git https://github.com/Callo42/gewu-multiverse
```

That needs a Rust toolchain (edition 2024, so Rust 1.85 or newer) and a Unix-like system.

## First run

```sh
gewu-multiverse
```

On the first start it writes a commented `config.toml` and a `credentials.toml` with mode 0600 under
your own configuration directory (`~/.config/gewu-multiverse` on Linux), and shows the risk
acknowledgment once. Press `y` to record it.

**You do not have to edit a file to begin.** On the Agents screen press `n` (or `a`, or `enter` on
the empty list) and fill in the form: an agent name, its model endpoint and model id, the model key,
and, if you have one, your Forgejo URL and token. The product writes both files for you and lists the
agent. Press `e` to write its instruction with your own `$EDITOR`, then `s` to start it. Both files
are still plain, commented TOML, and `r` re-reads them if you prefer to edit by hand.
`gewu-multiverse --config-dir <dir>` keeps the files somewhere else instead.

Keys: `?` (or `h`, or `F1`) shows every key; `q` quits, `tab` or `left`/`right` (or `1`-`6`) switch
screens, `up`/`down` choose an agent (or a run on the runs screen, or a repository on the Forgejo
screen), `enter` opens the chosen agent, `esc` goes back
to the list, `n`/`a` open the add-agent form, `s` starts the chosen agent, `x` stops it, `m` moves it
between the two
supervision modes (`tui` and `systemd`), `p` tests its model endpoint,
`c` checks the Forgejo role and `C` checks the same instance with the administrator token,
`i` settles the chosen agent's Forgejo identity, `g`
clones the chosen repository into its workspace, `P` opens a pull request as the agent,
`U` updates the newest open one, `e` edits its instruction, `K`
starts a new skill, `t` edits its declared run steps, `o` opens its newest log,
`T` opens its newest transcript,
and `r` re-reads the files. Each agent's own files live under
`<config dir>/agents/<name>/`; its runs are recorded under
`~/.local/state/gewu-multiverse/<name>/`. `--state-dir <dir>` moves the run records.

## Quickstart: from nothing to a running agent

This is the whole path, in the order it works. It uses a Forgejo you already run and any
endpoint that speaks the OpenAI-compatible chat-completions shape you can name; both may
be local. No secret is ever a command-line argument: the tokens and the model key go in
`credentials.toml`, whose mode is 0600.

1. **Install** (once) with the one line from the top of this README — it needs no Rust:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
   ```

   The prebuilt release is x86_64 Linux with glibc 2.39 or newer. On anything else — or to build
   it yourself — install from source instead, with Rust 1.85 or newer:

   ```sh
   cargo install --git https://github.com/Callo42/gewu-multiverse
   ```

2. **Start it once**: `gewu-multiverse`. It writes a commented `config.toml` and a
   `credentials.toml` (mode 0600) under `~/.config/gewu-multiverse`, and shows the risk
   acknowledgment over the Agents screen. Press `y` to record it; it is asked once per
   configuration directory. Quit with `q`.

3. **Add your first agent in the product.** On the Agents screen press `n` (or `a`, or
   `enter` on the empty list). A short form collects an agent name, the model endpoint
   and id, the model key, and — if you have one — the Forgejo URL and token. Press
   `enter` and the product writes both files for you and lists the agent. This is the
   path the product offers first; nothing here needs a text editor.

   If you would rather edit by hand, the form writes exactly this to
   `~/.config/gewu-multiverse/config.toml`:

   ```toml
   [forgejo]
   url = "http://127.0.0.1:3000"
   role = "member"

   [limits]
   run_timeout_secs = 900
   max_concurrent_agents = 10

   [[agents]]
   name = "scribe"
   model = "the-model-id"
   endpoint = "http://127.0.0.1:8000/v1/chat/completions"
   max_turns = 24
   instruction = "agents/scribe/instruction.md"
   concurrency = 1
   ```

   `instruction` is the path shown on the agent's screen. The run itself reads the
   derived `agents/<name>/instruction.md`, which is the file `e` writes and the file the
   tool checks before a start; point the field at that path to keep the two in step. See
   [How an agent runs](docs/agent-execution.md). Edit that file by hand and press `r` to
   re-read it.

4. **The credentials file** (mode 0600) holds secrets only. The form wrote the model key
   and, if you gave them, the Forgejo token or tokens. The file looks like this:

   ```toml
   forgejo_token = "…"          # the token the role check uses
   # forgejo_admin_token = "…"  # optional: create one account per agent (docs/forgejo.md)

   [model_keys]
   "scribe" = "…"               # the bearer key for this agent's endpoint
   ```

5. **Write its instruction**: press `e` to write
   `agents/scribe/instruction.md` through your own `$EDITOR`. The status line names the
   editor; set `$EDITOR` to choose another, or `~/.config/gewu-multiverse/config.toml`
   still works by hand.

6. **Check the Forgejo** with `c`, settle the agent's identity with `i`, and clone a
   repository with `g` (choose it with `up`/`down` on the Forgejo screen). With an
   administrator token the tool creates the account `gewu-<agent>` for you; without one,
   put the agent's own token under `[forgejo_agent_tokens]` first.

7. **Run it**: choose the agent and press `s`, or press `p` first to test the model
   endpoint on its own. The Agents screen moves the agent from `running` to `exited`.

8. **Read the result**: press `o` to open the newest `run.log` in your own `$PAGER`, or
   `T` for the append-only transcript beside it, at
   `~/.local/state/gewu-multiverse/<name>/<run-id>/transcript.jsonl`. The Runs screen
   (`6`) lists recent runs with their outcome and the failure reason.

One already-prepared run can be executed without the UI, which is also how the UI starts
an agent — as a child process of itself:

```sh
gewu-multiverse --run-agent ~/.local/state/gewu-multiverse/<name>/<run-id>
```

Two boundaries to know before the first run. First, the credentials file is created and
held at mode **0600**; the Credentials screen shows its path and its contents are never
printed. Second, an agent runs **as you** and a `sudo` attempt fails, but that is a
layout, not a sandbox: see [Running agents is not a sandbox](#running-agents-is-not-a-sandbox)
below and [Two ways an agent can be supervised](docs/systemd-mode.md) for how to make a
run outlive the window.

## Running agents is not a sandbox

Agents run **as you**, in their own workspace directory, with no privilege escalation — a `sudo` attempt fails. That is a
layout, not a security boundary: an agent can read anything your user can read, **including the credentials file**, and it acts
with your rights on this machine. The tool shows you where the credentials file lives, and asks you to acknowledge this once on
first start. A tool is started with the workspace as its working directory, and nothing more: the tool asks
the operating system for no isolation at all. Because an agent can read the credentials file,
every string the run writes down — a tool's output, a model reply, an error — is passed
through the redaction of the agent's own key and of every other secret in the credentials
file before it reaches the transcript or the log.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
