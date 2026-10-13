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

**Skeleton, and honest about it.** The crate builds, installs and opens the terminal UI. In place
now:

- the terminal UI, with a screen per thing you configure: the agent list, one agent's settings, its
  instruction and skills, the Forgejo connection and the role check, the credentials file, and runs;
- one configuration file, validated on load, with a clear message instead of a crash when it is
  wrong;
- one credentials file, created with mode **0600**, whose contents are never read, printed, logged or
  put on a command line;
- the first-run risk acknowledgment, required once per installation before any agent may be started.

Not there yet, and named as such on the screens that would show them: running an agent (so nothing
starts, stops or watches one), any network request, run records and transcripts, instruction and
skill handling, editing from the UI, and the Forgejo repository check.

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

Keys: `q` quits, `tab` or `left`/`right` (or `1`-`6`) switch screens, `up`/`down` choose an agent,
`enter` opens it, `esc` goes back to the list, `s` asks to start the chosen agent, `r` re-reads the
files.

## Running agents is not a sandbox

Agents run **as you**, in their own workspace directory, with no privilege escalation — a `sudo` attempt fails. That is a
layout, not a security boundary: an agent can read anything your user can read, **including the credentials file**, and it acts
with your rights on this machine. The tool shows you where the credentials file lives, and asks you to acknowledge this once on
first start.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
