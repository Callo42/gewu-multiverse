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

**Early.** The design is settled and the code is being written; nothing is installable yet. This README will grow the
install command and a quickstart when the first binary exists.

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
