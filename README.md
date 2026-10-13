# gewu-multiverse

A **local, single-Universe Matrixverse**: one small terminal application that turns a Linux machine into a place where
you run **agents against your own Forgejo** — no server, no accounts, no cloud.

You install it, open the terminal UI, and it lets you:

- point it at **one Forgejo** you already have (URL + token, and which role to act as — an admin token can see
  administrative views, an ordinary one cannot);
- define **how many agents** you want, each with its **own model endpoint, key and instructions**;
- use the agent identities and tokens so that an agent's commits are attributed to that agent, never to you;
- start, stop and watch agents, with a log and a transcript for every run;
- check the repositories you can reach — under the role you configured.

## What this deliberately is not

By design there are **no users, tenants or roles**, **no multiple GitLab instances**, **no provisioning of anything**,
**no complex agent lifecycle**, **no ATIF capture**, **no web interface or API server**, and **no cloud dependencies**.
The terminal UI and two files on disk — a configuration file and a credentials file — are the whole interface. Run
records are plain logs and JSONL transcripts.

## Status

**An agent runs; the model and the Forgejo are the next rounds.** In place now:

- the terminal UI, with a screen per thing you configure: the agent list and each
  agent's state, one agent's settings, its instruction and skills, the Forgejo
  connection and the role check, the credentials file, and the recent runs;
- one configuration file, validated on load, with a clear message instead of a crash
  when it is wrong — including the run timeout and the limit on agents running at once;
- one credentials file, created with mode **0600**, whose contents are never read,
  printed, logged or put on a command line;
- the first-run risk acknowledgment, required once per installation before any agent
  may be started;
- **agents that actually run**: one child process per agent, in its own workspace, with
  an instruction, a skill set and a declared list of shell steps; started, stopped and
  watched from the UI, within a configurable concurrency limit and under a configurable
  timeout; each run leaving a log and an append-only JSONL transcript under
  `~/.local/state/gewu-multiverse/<agent>/<run-id>/`;
- editing an agent's instruction, its skills and its declared steps from the UI, through
  your own `$EDITOR`, and reading a run's log through your own `$PAGER`.

Not there yet, and named as such on the screens that would show them: **the model
call** (a run follows its declared steps; it does not decide them) and **the Forgejo
work** (no request is made, no role is checked, and a workspace is a plain directory
rather than a clone). See [How an agent runs](docs/agent-execution.md) for the details.

## Install

```sh
cargo install --git https://github.com/Callo42/gewu-multiverse
```

That needs a Rust toolchain (edition 2024, so Rust 1.85 or newer) and a Unix-like system.

## First run

```sh
gewu-multiverse
```

On the first start it writes a commented `config.toml` and a `credentials.toml` with mode 0600 under
your own configuration directory (`~/.config/gewu-multiverse` on Linux), shows the risk
acknowledgment once, and asks you to confirm it. Edit the configuration file, then press `r` to
re-read it. `gewu-multiverse --config-dir <dir>` keeps the files somewhere else instead.

Keys: `q` quits, `tab` or `left`/`right` (or `1`-`6`) switch screens, `up`/`down` choose
an agent (or a run on the runs screen), `enter` opens the chosen agent, `esc` goes back
to the list, `s` starts the chosen agent, `x` stops it, `e` edits its instruction, `K`
opens its skills directory, `t` edits its declared run steps, `o` opens its newest log,
and `r` re-reads the files. Each agent's own files live under
`<config dir>/agents/<name>/`; its runs are recorded under
`~/.local/state/gewu-multiverse/<name>/`. `--state-dir <dir>` moves the run records.

## Running agents is not a sandbox

Agents run **as you**, in their own workspace directory, with no privilege escalation — a `sudo` attempt fails. That is a
layout, not a security boundary: an agent can read anything your user can read, **including the credentials file**, and it acts
with your rights on this machine. The tool shows you where the credentials file lives, and asks you to acknowledge this once on
first start. A run's declared steps are started with the workspace as their working directory, and nothing more: the tool asks
the operating system for no isolation at all.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
