# Connecting a Forgejo

`gewu-multiverse` works with **one** Forgejo. There is no relay and no anonymous call:
every request carries a token read from the credentials file, and the tool says plainly
what that token can see. This document is about that connection: where the URL, the
token and each agent's identity live, what the role check means, and how an agent's
commits come to be authored by the agent rather than by you.

## Where the Forgejo lives

Two files, both under your configuration directory (`~/.config/gewu-multiverse` on
Linux, or whatever `--config-dir` names):

```toml
# config.toml — no secret at all
[forgejo]
url = "https://forgejo.example.org"
role = "member"        # "admin" or "member"
```

```toml
# credentials.toml — secrets only, created with mode 0600
forgejo_token = ""          # the token the role check uses
forgejo_admin_token = ""    # optional; see "One identity per agent" below

[forgejo_agent_tokens]      # used when no admin token is configured
# "scribe" = ""
```

The tool never asks for a token on the command line, and it never prints one. A token
read from this file is held in a type that formats as `<redacted>`, and every message
built from a Forgejo response is passed through that redaction, because a refusal can
echo what was sent.

## The role check

Press `c`. The tool does three things and shows what happened:

1. **Checks the URL.** A base URL must start with `http://` or `https://`; anything
   else fails as `bad_url` before a request is made.
2. **Asks who the token is** (`GET /api/v1/user`) and whether that account is an
   administrator. A token the instance does not accept fails as `bad_token`.
3. **Lists the repositories the token can see** (`GET /api/v1/repos/search`) with, for
   each one, its `namespace/name`, whether it is private or public, and what you
   may do with it — `read`, `write` or `admin`.

The `role` in the configuration is what the token is *expected* to be. With
`role = "admin"`, a token that belongs to an ordinary account is refused as `not_admin`
rather than silently showing the ordinary view. With `role = "member"`, any valid token
is accepted.

The difference between the two views is visible in the list:

- an **administrator's** token sees every repository on the instance, including private
  repositories it does not own — the administrative view;
- an **ordinary** token sees the public repositories and the private ones it has been
  given.

If `forgejo_token` is empty and `role = "admin"`, the role check uses
`forgejo_admin_token` instead, so one token can serve both purposes.

## One identity per agent

An agent works as *itself*, not as you. Which account that is comes from the
configuration and the credentials file, never from an edit to the program:

- **With an administrator token** (`forgejo_admin_token`), the tool creates and manages
  one Forgejo account per agent. The account's login is `gewu-<agent>`, derived from the
  agent's own name. The tool writes a mark on the account (so a later run can tell its
  own machine identity from a person who happens to have that name), refuses to take
  over an account that does not carry the mark, sets the account a freshly generated
  password for the length of one request — the password is never stored — makes a token
  for it, and writes that token under `[forgejo_agent_tokens]`. This is the `managed`
  path.
- **Without** an administrator token, you put each agent's token under
  `[forgejo_agent_tokens]`. The tool checks it against the Forgejo, learns whose it is,
  and records that. This is the `supplied` path.

Either way the credentials file ends up recording the login and the email a commit will
use, and the mode in use is recorded so the screens can say which path you are on.
Configuration — the list of agents and the presence of an administrator token — decides
which identities exist; no source is edited.

An agent's identity is settled by pressing `i` on the agent screens, or automatically
the first time it is needed.

## Working on a repository

After a role check, the Forgejo screen lists the repositories the token can see. Choose
one with `up`/`down` and press `g`: the repository is cloned into the agent's workspace
at `<config dir>/agents/<agent>/workspace/<repository>`, and the agent's git identity —
the login and email recorded above — is written into that clone's local configuration.
If the clone is already there, `g` opens it and re-applies the identity rather than
cloning again.

The agent uses **the user's own `git`**; the tool needs no `glab` and no other Git host
client. `P` pushes the workspace's current branch and opens a pull request as the agent;
`U` updates the newest open pull request. A title and body for the pull request can be
put in `<config dir>/agents/<agent>/pull-request.txt` — the first line is the title, the
rest is the body.

### Authoring, not spoofing

A commit made in the workspace is authored by the agent because git's local
`user.name`/`user.email` for that repository are the agent's. That is ordinary git
configuration, not a forged field: the agent's account really exists on the Forgejo,
its token really is the one used to push, and the instance attributes the commit to it.

### How git gets the token

git is started with `GIT_ASKPASS` pointing back at this same program. When git needs a
password it runs that program with the prompt as its one argument; the program reads the
agent's token from the credentials file and writes it to standard output, which is the
pipe git reads. The login travels in the clone URL; the secret does not. So the token is
never a URL, a command-line argument, a log line or a screen. `credential.helper` is
disabled for every git command the tool runs, so the token is not copied into a git
credential store either.

## Trying it against a local instance

The integration test drives a throwaway Forgejo — its own temporary root, its own
SQLite database, its own port on `127.0.0.1` — and tears it down afterwards. Download a
Forgejo release binary, then:

```sh
GEWU_FORGEJO_BIN=/path/to/forgejo cargo test --test forgejo -- --nocapture
```

Without `GEWU_FORGEJO_BIN` the test says so and does nothing; the repository does not
carry a server binary. The test covers the role check under an administrator's token
and an ordinary one, identity creation, a commit authored by the agent's identity,
opening and updating a pull request, a refused operation outside the token's rights, and the
assertion that no token appears in anything the tool produces.
