# The bundled Forgejo

`gewu-multiverse` brings **one** Forgejo and runs it for you. There is no relay and no
anonymous call: every request uses a key read from the credentials file. This document
is about that instance: how it is installed, where its files are, how the person's
account and the agents' accounts are made, and how an agent works in it.

## Installing it

The release is pinned: Forgejo 17.0.0 for Linux amd64, SHA-256
`5a3b07e124ed70b4e46ee3939515f003bd3cc85352257a9a4607a527251042ca`. The installer
streams a source to a temporary file beside the target, hashes what it wrote, and
renames the file into place only when the digest matches; a mismatch removes the
temporary file and keeps nothing.

The sources are tried in order: this release's own GitHub asset (tagged with the crate
version), then the official Codeberg release. A missing or wrong first source falls
through to the second with no question. Only when the whole list fails does the install
refuse, keep nothing, and say the next start tries again. `GEWU_FORGEJO_BINARY` names a
local file whose SHA-256 is checked the same way, for tests.

The first start installs the binary by itself when it is missing, and the build picture
reports `downloading n%`. `gewu-multiverse --install-forgejo` runs the installer on its
own; `install.sh` calls it once.

## Where the instance lives

Everything is under the data root: `~/.local/share/gewu-multiverse` on Linux, or
`$XDG_DATA_HOME/gewu-multiverse`.

```
forgejo/
  forgejo-17.0.0-linux-amd64   0755, hash-checked
  engine.lock                 0600, one program per data directory
  instance/
    app.ini                   0600, generated secrets and SQLite
    secrets/admin-password    0600, generated once
    secrets/admin-login       0600
    log/forgejo.log
    data/forgejo.db
    data/repositories/
```

The address is chosen once from 3000–3099, written into `app.ini`, and read back on
every later start. It is reachable only from this computer. The person's administrator
login comes from `$USER` when that is a valid Forgejo login, else `owner`; the board
shows it next to `your login:`. The generated password is never printed by the engine;
the Forgejo page shows it for a moment when the person presses `p`, and the file above
holds it with mode 0600.

`migrate` runs before the first administrative change, and the administrator account is
made through Forgejo's own `admin user` CLI. The server runs as `web` in its own process
group. It is ready when `/api/v1/version` answers.

## What is set up

Provisioning through the administration API is idempotent. It makes:

- organisation `team`;
- team `agents`, with write access to every repository in the organisation and
  write units for code, issues, pull requests and projects, so Forgejo's own summary
  also says `write`;
- the starter repository `team/workspace`, with a README.

The engine holds a lock at `forgejo/engine.lock` for as long as it is open. A second
program pointed at the same data root fails at open with one sentence and touches
nothing.

The administration key is kept in the credentials file as `forgejo_admin_token`. The
product makes it with Forgejo's own CLI, checks it against `/api/v1/user`, and reuses
it while it still names the administrator. It is never printed.

## One account per agent

Each agent works as itself. Its account's login is `gewu-<agent>` — the agent's own
validated name with the product's prefix — and its email is
`gewu-<agent>@agents.invalid`, a reserved domain that can never receive mail.

The engine's `identity::manage`:

1. finds or creates the account;
2. writes a mark on it (`Machine identity created by gewu-multiverse.`), and refuses to
   take over an account without the mark, so a person who happens to have the name is
   never captured;
3. sets a freshly generated password for the length of one request and never stores
   it;
4. makes a token for the account, named after it, with the rights the clone and the
   issue tools need;
5. proves the new token can read assigned issues before recording the rights marker;
6. revokes the token a renewal replaces, on the server, so an old key stops answering;
7. records the login, email, mode (`managed`), creation time, rights marker and token
   name in the credentials file.

The agent's token lives under `[forgejo_agent_tokens]` in the 0600 credentials file.
Git is handed the token through `GIT_ASKPASS`: git runs this same program with the
prompt as its one argument, and the program answers from that file over a pipe. The key
is never a URL, a command-line argument, a log line or a screen. `credential.helper` is
disabled for every git command the product runs, so the key is not copied into a git
credential store.

The account, its commits and its run records stay when the agent is removed. Removal
deletes the token on the server by its recorded name, removes the account from team
`agents`, deletes the key from the credentials file, and forgets the agent in the
settings. It is not a history rewrite.

## Working in the repositories

Before each run, every repository of `team` (at most 20) is cloned into the agent's
workspace as the agent, or fetched if it is already there. A present clone is never
reset, checked out or discarded; local work stays. The agent's git identity — the
recorded login and email — is written into the clone's local configuration, so a
commit made there is authored by the agent because the account really exists and its
key really pushes the commit.

An agent uses the user's own `git`; no `glab` and no other Git host client is needed.
The run declares six Forgejo tools beside `shell` only when all three hold: a Forgejo
context was written for the run, the agent's identity is settled with its own key in
the credentials file, and at least one repository is cloned into its workspace.
Otherwise the run declares `shell` alone.

| Tool | Arguments | Authority |
|---|---|---|
| `forgejo_list_assigned_issues` | none | read the open issues assigned to the agent's account in its cloned repositories |
| `forgejo_read_issue_or_pull` | `repository`, `number` | read one issue or pull request and its comments, bounded and with the cut declared |
| `forgejo_comment` | `repository`, `number`, `body` | one comment on an issue or pull request |
| `forgejo_list_my_pull_requests` | none | read the agent's own open pull requests |
| `forgejo_open_pull_request` | `repository`, `title`, `body` | open a request from the clone's checked-out branch |
| `forgejo_update_pull_request` | `repository`, `number`, `title`, `body` | change the agent's own open request for that branch |

The repository is `owner/name` and must be one of the agent's clones; anything else is
refused with one sentence naming the ones it may use. Opening refuses when the branch
is the base, has no commits the base lacks, is not on the remote, or already has an open
request for the pair. Updating changes only the agent's own open request for that
branch and repository, as the recorded login. The tools never merge, close, reopen,
delete, label, assign, approve a review, or change a repository or a user.

A comment, an opened request and an update are writes and each counts against a
**per-run bound of eight writes**; after that the write tools refuse with one sentence,
while reads continue. Every call is made with the agent's own key, read at the call. The
key is not in a model message, a tool result, the transcript, the log, `shell`'s
environment or the workspace. Every failure reaches the model as one bounded sentence
naming the class and status, never a raw reply body and never a key.

Each call is recorded in the run's transcript with its tool, repository, number and
outcome; each successful write is also a run action with its web URL. The board reads
the newest action for the agent's card.

## Trying it against a local instance

The live tests are `#[ignore]`d, so the ordinary suite lists them as ignored and never
counts them as passed. Run them explicitly with the pinned binary:

```sh
GEWU_FORGEJO_BIN=/path/to/forgejo-17.0.0-linux-amd64 \
  cargo test --test forgejo -- --ignored --nocapture --test-threads=1
```

The live tests run one at a time: each starts its own instance in the same small
port range, and two instances racing for one port would test the wrong server. Each
test starts its own throwaway instance in a temporary root, on its own port, and
tears it down afterwards. It covers the administrator's account and the repositories
it can see, an ordinary account's narrower view, a bad URL and a bad key, one managed
agent identity with a private credentials file, a commit authored by the agent, a push,
an opened and updated pull request, a refusal outside the ordinary account's rights,
none of the keys appearing in anything the tool produces, and the renewal that revokes
the key it replaces.

The engine tests exercise the whole first run (download, prepare, start, two agent
accounts, a run, a second run, shutdown) and the first run after a reopen (the agent is
left started, Forgejo is stopped and started again, and the first recovered run must
carry `forgejo.json` and the six tools) against the same kind of local instance:

```sh
GEWU_FORGEJO_BINARY=/path/to/forgejo-17.0.0-linux-amd64 \
  cargo test --test engine-bundled-forgejo -- --ignored --nocapture --test-threads=1
```
