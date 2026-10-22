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
    app.ini                   0600, generated secrets, SQLite in WAL mode
    secrets/admin-login       0600, the login the instance was made with
    custom/templates/base/head_script.tmpl   the locale guard before the pinned page script
    log/forgejo.log
    data/forgejo.db
    data/repositories/
```

The address is chosen once from 3000–3099, written into `app.ini`, and read back on
every later start. It is reachable only from this computer. A new installation's
administrator login is the fixed `owner`, not the operating system account name; the
board shows it next to `your login:`. Forgejo reserves `admin` for its own route, so
`owner` is the fixed choice. An installation made by an earlier version keeps its
recorded login and the page says `(kept from an earlier setup)`.

The administrator password is not written down at all and never appears in a process
argument. The account is created with Forgejo's CLI `--random-password`; the value is
read only to be discarded. `p` on the Forgejo page resets it through the administration
API — the value is in the request body, never in a process argument — and shows the new
value, wrapped whole, for ten seconds (deliberately two lines at 80 columns). The
consequence is a static line on the page: the password shown before the reset no
longer works.

The database runs in WAL mode with `SQLITE_TIMEOUT = 5000` (five seconds), and the
API client gives each call ten seconds — well under a run's own lifetime — so two
agents writing at once wait for the lock rather than hanging the card on a 500.

Those pinned keys are checked on **every** start, not only when `app.ini` is created:
an instance made by an earlier version keeps `SQLITE_TIMEOUT = 500`, no journal mode,
and no `[i18n]`/`[time]`/`[other]` section, so the keys are replaced or added before
Forgejo is started. The pinned `[other] SHOW_FOOTER_TEMPLATE_LOAD_TIME = false` hides
the page-footer timing that made a stranger's first page look like a debug build, on a
new instance and on one an earlier version made. Every other setting the person wrote
stays exactly as it is.

`migrate` runs before the first administrative change, and the administrator account is
made through Forgejo's own `admin user create --random-password` CLI. The server runs
as `web` in its own process group. It is ready when `/api/v1/version` answers.

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
prompt as its one argument. In a confined run the program reads a per-run file that
holds only that run's login and token, under the run's private `TMPDIR` (private from
other runs on every mechanism); outside a run it reads the shared credentials file over
the same pipe. The per-run file and its whole temporary tree are removed on every exit
the run sees, by the engine when it had to stop, kill or reap a crashed run, and by the
next open when the owner is gone. The shared file — the
model key, the administrator token and every other agent's login — is not readable
inside a confined run. The key is never a URL, a command-line argument, a log line or
a screen. `credential.helper` is
disabled for every git command the product runs, so the key is not copied into a git
credential store.

Because `git` inside a run must push, the run holds its own Forgejo account's token,
and a model can therefore learn that token. It reaches only that one account on the
person's loopback Forgejo; removal deletes it and deactivates the account, so it stops
working. The administrator credential, the other agents' tokens, the person's files
and the product's own state are out of the run's reach; the security audit proved each.

The account, its commits and its run records stay when the agent is removed. Removal
deletes the token on the server by its recorded name, removes the account from team
`agents`, deactivates the account (`active = false` and `prohibit_login = true`, so a
web sign-in is refused as well as every token), deletes the key from the credentials
file, and forgets the agent in the settings. It is not a history rewrite; the
confirmation sentence says all four things before it acts. `+ add` never reuses a
removed name: the number high-water mark is kept, so the next agent gets a new number,
a new login and a new account; the removed account stays deactivated with its history.

## Working in the repositories

Before each run, every repository of `team` (at most 20) is cloned into the agent's
workspace as the agent, or fetched if it is already there. A present clone is never
reset, checked out or discarded; local work stays. The agent's git identity — the
recorded login and email — is written into the clone's local configuration, so a
commit made there is authored by the agent because the account really exists and its
key really pushes the commit.

An agent uses the user's own `git`; no `glab` and no other Git host client is needed.
The work rule is the assignment: the list tool shows only the open issues assigned to
the agent's own account in its cloned repositories, and a write outside that scope is
refused by Forgejo. `forgejo_read_issue_or_pull` can open any issue or pull request by
number in a cloned repository (a read is not a claim on it); the page therefore says
`agents work only on issues assigned to them`, not that reads are invisible. The page
and the README say so, and the Agent screen's empty work area says `no issues assigned
to gewu-agent-1 yet`. Assign an issue to the agent's Forgejo
account first; an unassigned issue is not silently picked up.
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
the newest action for the agent's card. A failed call leaves one visible line on the
card and in the run log; the card prefixes it with the newest run's mark (`⚠` for a
warned run, `✗` for a failed one), the log keeps `✗ <tool>: <first clause of the error>`,
and while a call is in
flight the card says which tool it is and how many seconds it has waited.

## The bundled pages' language

Forgejo's `webcomponents.js` builds an `Intl.DateTimeFormat` from
`navigator.language` at load time. A browser whose language tag is not usable (`C`, an
empty string, a tag older ICU rejects) makes that throw before the page's own script
runs, and Forgejo paints its red `JavaScript error` banner on every page. A browser is
not the product's to configure, so the pinned instance serves one custom template,
extracted from the pinned binary and prefixed with a small guard: an unusable language
is replaced with `en-US`; a valid one is left alone. `app.ini` also pins a valid
default (`[i18n] LANGS = en-US`, `[time] DEFAULT_UI_LOCATION = UTC`), so the page's own
language and date formats stay valid even when the browser's `Accept-Language` is odd.

Registration is off (`DISABLE_REGISTRATION = true`), so `/user/sign_up` renders
`Registration is disabled. Please contact your site administrator.` for a signed-out
visitor. Forgejo itself redirects a visitor who is already signed in from that route to
the dashboard; there is no setting that keeps that page for a signed-in account, and
this build does not pretend otherwise.

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
