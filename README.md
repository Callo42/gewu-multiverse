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

**An agent runs and thinks; the Forgejo is the next round.** In place now:

- the terminal UI, with a screen per thing you configure: the agent list and each
  agent's state, one agent's settings, its instruction and skills, the Forgejo
  connection and the role check, the credentials file, and the recent runs;
- one configuration file, validated on load, with a clear message instead of a crash
  when it is wrong — including the run timeout, the limit on agents running at once,
  and each agent's bound on model turns;
- one credentials file, created with mode **0600**, which holds the model keys. A key
  is read only at the moment of a model call, and every transcript, log and error the
  tool writes has it redacted;
- the first-run risk acknowledgment, required once per installation before any agent
  may be started;
- **agents that actually run**: one child process per agent, in its own workspace, with
  an instruction, a skill set and a declared list of shell steps; started, stopped and
  watched from the UI, within a configurable concurrency limit and under a configurable
  timeout; each run leaving a log and an append-only JSONL transcript under
  `~/.local/state/gewu-multiverse/<agent>/<run-id>/`;
- **a direct model call**: a run posts one non-streaming chat request to the endpoint
  its agent's configuration names, with the model id and the key from the credentials
  file. It runs the tool calls the model asks for inside the workspace, feeds the
  results back, and repeats until the model answers without a tool call or the declared
  turn bound is reached. Every exchange is in the transcript as it happened, with the
  key redacted;
- **"test this endpoint"** on an agent's screen (`p`): one minimal call, and the answer
  shown plainly — either what the model replied, or the class of failure;
- editing an agent's instruction, its skills and its declared steps from the UI, through
  your own `$EDITOR`, and reading a run's log through your own `$PAGER`.

Not there yet, and named as such on the screens that would show it: **the Forgejo
work** (no request is made, no role is checked, and a workspace is a plain directory
rather than a clone). See [How an agent runs](docs/agent-execution.md) for the details.

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
to the list, `s` starts the chosen agent, `x` stops it, `p` tests its model endpoint,
`e` edits its instruction, `K`
opens its skills directory, `t` edits its declared run steps, `o` opens its newest log,
and `r` re-reads the files. Each agent's own files live under
`<config dir>/agents/<name>/`; its runs are recorded under
`~/.local/state/gewu-multiverse/<name>/`. `--state-dir <dir>` moves the run records.

## Running agents is not a sandbox

Agents run **as you**, in their own workspace directory, with no privilege escalation — a `sudo` attempt fails. That is a
layout, not a security boundary: an agent can read anything your user can read, **including the credentials file**, and it acts
with your rights on this machine. The tool shows you where the credentials file lives, and asks you to acknowledge this once on
first start. A tool is started with the workspace as its working directory, and nothing more: the tool asks
the operating system for no isolation at all. Because an agent can read the credentials file,
every string the run writes down — a tool's output, a model reply, an error — is passed
through the key's redaction before it reaches the transcript or the log.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
