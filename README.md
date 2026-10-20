# gewu-multiverse

**One Forgejo on your own computer, and the agents you run on it — drawn as one picture in your terminal.**

You install one small program. It brings its own Forgejo and keeps it running for you.
You choose a model, choose how many agents you want, and write each one an instruction.
Every agent has its own login on your Forgejo, so its commits, comments and pull
requests are its own. The board shows the whole thing at a glance:

```
╭─╴ gewu-multiverse ╶──────────────────────────────────────────────────────────────────────────────╮
│                                                                                                  │
│                                                                                                  │
│                       ╭╴ FORGEJO ╶─────────────────────────────╴ ● running ╶─╮                   │
│                       │                                                      │                   │
│                       │ http://127.0.0.1:3000                                │                   │
│                       │ team · 1 repository                                  │                   │
│                       │ 3 agents                                             │                   │
│                       │ version 17.0.0                                       │                   │
│                       │ your login: admin                                    │                   │
│                       ╰───────────────────────────┬──────────────────────────╯                   │
│                                                   │                                              │
│                  ╭────────────────────────────────┼────────────────────────────────╮             │
│ ╭╴ ○ agent-1 ╶───┴───────────╮   ╭╴ ○ agent-2 ╶───┴───────────╮   ╭╴ ○ agent-3 ╶───┴───────────╮ │
│ │ gewu-agent-1               │   │ gewu-agent-2               │   │ gewu-agent-3               │ │
│ │ stopped                    │   │ stopped                    │   │ stopped                    │ │
│ │                            │   │                            │   │                            │ │
│ ╰────────────────────────────╯   ╰────────────────────────────╯   ╰────────────────────────────╯ │
│                                                                                                  │
│ ←→ pick   ⏎ open   s start/stop   + add   − remove                                               │
│ f Forgejo   m model   ? help   q quit                                                            │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
```

## What it is

- **One Forgejo, on your machine.** The program downloads a fixed Forgejo release,
  starts it, and gives you an administrator login. Nothing is hosted anywhere else,
  and the address is reachable only from this computer.
- **Agents with their own accounts.** Each agent is a Forgejo account named
  `gewu-<agent>`. When it commits, comments or opens a pull request, Forgejo shows the
  agent as the author, not you.
- **One model for all of them.** You give one model address and one key. Every agent
  uses them, with its own instruction.
- **A card for each agent.** A filled circle means working, a half circle means waiting
  for its next run, an empty circle means stopped. The card also shows its Forgejo
  login and its latest run.

The program keeps the Forgejo running while it is open. When an agent is started, it
runs its instruction, and when that run ends it starts another one, until you stop it.

## Install

One line, no Rust:

```sh
curl -fsSL https://raw.githubusercontent.com/Callo42/gewu-multiverse/main/install.sh | sh
```

It downloads the current prebuilt program to `~/.local/bin/gewu-multiverse`, and the
Forgejo it needs (about 122 MB) to `~/.local/share/gewu-multiverse/`. If `~/.local/bin`
is not on your `PATH`, the installer says how to add it. The prebuilt program is for
64-bit x86 Linux with glibc 2.39 or newer (for example Ubuntu 24.04 or newer).

From source, with Rust 1.85 or newer:

```sh
cargo install --git https://github.com/Callo42/gewu-multiverse
```

Then run:

```sh
gewu-multiverse
```

## The four steps

On the first start the board walks you through these; after that it opens on the board
and you can change any of them later.

1. **Read the five warnings.** They say that agents use this computer with your
   permissions and can read your files, including saved keys. Press `y` after you have
   read every one.
2. **Set the model.** Give the full model address (it ends in `/chat/completions`), the
   model name, and the key. Press `t` to test it; `✓ the model answered` means it
   works. Press `⏎` to go on.
3. **Choose how many agents.** `←` / `→` changes the number from 1 to 12. Press `⏎` to
   create them. The board shows `downloading n%`, `setting up`, `starting`, then
   `● running` while your Forgejo is prepared.
4. **Write each agent's instruction.** Pick a card with `←` / `→` and press `⏎`, then
   `e`. Your own editor opens `instruction.md`; write what the agent should do and
   save. Press `s` to start it. Press `s` again to stop it.

After the first setup, `+` adds an agent and `−` removes the selected one (it asks
once). `m` opens the model settings, `f` opens the Forgejo page, `?` lists every key,
and `q` quits.

## Where things live

| What | Where |
|---|---|
| The program | `~/.local/bin/gewu-multiverse` |
| Your settings (model address and name, Forgejo login, agent names) | `~/.config/gewu-multiverse/multiverse.toml` |
| Your saved keys | `~/.config/gewu-multiverse/credentials.toml` (mode 0600, yours alone) |
| Each agent's instruction, skills and workspace | `~/.config/gewu-multiverse/agents/<agent>/` |
| Every run's log and record | `~/.local/state/gewu-multiverse/<agent>/<run>/` |
| Forgejo and its database | `~/.local/share/gewu-multiverse/forgejo/` |
| The Forgejo administrator password | `~/.local/share/gewu-multiverse/forgejo/instance/secrets/admin-password` (mode 0600) |

The Forgejo address is on the board and on the Forgejo page, for example
`http://127.0.0.1:3000`. On the Forgejo page (`f`) your login is shown next to
`your login:`, and the board shows it too when the window has room. The password is
not printed: open the Forgejo page and press `p` to show it for a moment, or read the
file above.

`--config-dir <dir>` moves the settings, keys and agent folders; `--state-dir <dir>` moves the run records.
A relative folder is taken from the folder you started the program in.
`--demo` opens the board with a scripted example and no real Forgejo.

## Stopping, quitting, and opening again

- `s` stops the selected agent. Its current run is asked to stop, and then made to stop
  if it does not; nothing new starts.
- `q` quits. If an agent is started, the board first says how many will stop; press `q`
  again. It stops every started agent and the Forgejo, and prints one line. Closing the
  terminal window does the same.
- Opening the program again starts the Forgejo, and starts the agents you had left
  started, with the same instructions.
- If the program was killed outright, the next start finds the Forgejo and any runs it
  left behind, stops them, and only then opens. It believes a saved process number only
  when the command line still matches what it started.

## Removing it

The program is one file; removing it is the whole uninstall:

```sh
rm ~/.local/bin/gewu-multiverse
```

Your data is in three folders. Removing them removes the Forgejo, its database, your
settings, your agents' files and every run record:

```sh
rm -rf ~/.config/gewu-multiverse ~/.local/state/gewu-multiverse ~/.local/share/gewu-multiverse
```

Nothing else is installed: no service, no system-wide file, no other account.

## When something goes wrong

- **The Forgejo download did not finish.** The board says
  `could not download Forgejo — check the internet connection; the next start tries again`.
  Check the connection and start the program again, or run
  `gewu-multiverse --install-forgejo` and watch the percentage. Nothing partly
  downloaded is kept.
- **The model test fails.** The board gives one line:
  `✗ wrong address — it should end in /chat/completions` (the address must be the full
  completions address), `✗ the API key was refused` (check the key),
  `✗ no model called <name> at this address` (check the model name), or
  `✗ no answer within 15 s` (the provider did not answer; try again).
- **An agent keeps failing.** Its card shows `next run in Ns` and its last runs as
  `✓` and `✗`. Open it with `⏎`; `l` opens the newest log and `t` the full record in
  your pager. The wait between attempts starts at 10 seconds and doubles up to 5
  minutes, so a broken instruction does not spin. Fix the instruction with `e` or the
  model with `m`, then stop and start the agent with `s`.
- **Forgejo stops unexpectedly.** The board says
  `Forgejo stopped unexpectedly — restarting it` and tries again, waiting a little longer
  each time. If it says `stopped: …` while it is still setting up, press `r` to try now.
- **Another copy is already open.** The program uses one data folder at a time and says
  so; close the other copy first.

## What it does not do

- It is **not a sandbox**. Agents run as you, with your permissions. They can read your
  files, including `credentials.toml`. The program removes saved keys from the logs and
  records it writes, but a command an agent runs can still do anything you can do.
- It does not connect to another Forgejo, and it does not host more than one.
- It does not let you open or update pull requests from the terminal. The agents do
  that in Forgejo; you review there in a browser.
- It has no scheduler beyond "keep running until stopped", no separate web page or API
  beyond the bundled Forgejo, no cloud service, and no user accounts other than your own
  and the agents'.
- It does not expose the Forgejo to your network. It listens on this computer only.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
