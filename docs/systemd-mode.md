# Two ways an agent can be supervised

`gewu-multiverse` starts agents in one of two ways. The default is a **child of the
terminal UI**. The other is **opt-in, per agent**: a **user unit** of your own
`systemd`, which keeps running when the UI exits.

Both ways run the same program the same way: a run reads a plan written before it
starts, works in the agent's own workspace, reads the model key from the credentials
file, and leaves the same `run.log` and `transcript.jsonl` under the state directory.
The two differ only in who starts the process and in who stops it.

## The default: a child of the terminal UI

Nothing has to be configured for this. The tool starts one child process per agent, in
its own process group, with the agent's workspace as its working directory. When you
leave the UI, every child is stopped and reaped, so nothing of a run is left behind. If
you close the terminal window, the agent stops with it.

This is the right default: a run belongs to the session you started it in, and the
tool can prove the process is gone when it leaves.

## The opt-in: a user unit

For work that should outlive the window it was started from, an agent can run as a
**user unit**. The tool writes a unit file under your own systemd directory and asks
your own `systemctl --user` to start it.

- The unit is written to `$XDG_CONFIG_HOME/systemd/user/gewu-agent-<name>.service`
  (usually `~/.config/systemd/user/`). It is never a system unit, and nothing here runs
  as root.
- The unit's `ExecStart` is exactly the command the UI would run —
  `gewu-multiverse --run-agent <run dir>` — with the agent's workspace as
  `WorkingDirectory`.
- The unit holds **no secret**. A key or a token is never written into it: the run reads
  the credentials file itself, at the moment it needs the value.
- The run's own log and transcript are the same files the default path produces. The
  unit's standard output and standard error are sent to that same `run.log`, so nothing
  a run prints is lost when there is no terminal.
- Because the unit belongs to systemd, not to the UI, **leaving the UI does not stop
  it**. Stopping is done with `systemctl --user stop`, from the UI (`x`) or by hand.

### Switching modes

Press `m` on the agent list or an agent's screen to move the chosen agent to the other
mode. The move is **idempotent**: choosing the mode an agent is already in says so and
changes nothing. A move is **refused while the agent is running**, in either mode, with
a sentence naming the key that stops it first. Moving an agent out of systemd mode
removes its unit file and clears any failed state, so the machine keeps no unit for an
agent the record no longer points at.

The mode is recorded per agent, in `agents/<name>/supervision.toml` under the
configuration directory. A record that is absent, unreadable or malformed means the
default mode: a record the tool cannot understand never causes a unit to be started.

### Restarts

A unit restarts a run that failed, within a declared bound:

```ini
[Unit]
StartLimitIntervalSec=60
StartLimitBurst=3

[Service]
Restart=on-failure
RestartSec=2
```

At most three starts in sixty seconds; after that the unit is left failed and the agent
list shows it as failed. Every restart systemd reports is written into the **run's own
log**, as a line like

```text
2026-01-01T00:00:00Z supervisor: systemd restarted this run (restart 1 of at most 3)
```

so the record says a restart happened instead of hiding it. A restart re-runs the same
command against the same run, which is why the restart is visible in that run's record
rather than in a new one.

### What the machine needs

`systemctl --user` needs a **user session**: a systemd instance running as you. On most
desktops that is always there. It is not there on a bare container or a serial console
with no login manager, and on such a machine the tool will refuse to start a
systemd-mode agent with a sentence saying so — it never silently falls back to the
default mode. The tool asks the user's systemd whether it answers before it writes a
unit.

To keep an agent running **after you log out**, the user's systemd must be allowed to
run without a login session. That is one command, run once:

```sh
loginctl enable-linger "$USER"
```

Without lingering, a unit survives the UI exiting and the terminal closing, but it is
stopped when the user's last session ends.

### What this mode is not

It is a **convenience, not a security boundary**. A unit runs as you, in the same
workspace, with the same rights as a child process: no sandbox, no privilege change, no
isolation, nothing you did not already have. It also does not make the tool a service
manager: the tool writes units for its own agents and nothing else, and it never
installs, enables or manages a system unit. It does not enable a unit for you — starting
an agent is a start, not a login-time service. If you want a unit to start at login,
`systemctl --user enable gewu-agent-<name>.service` is the command, and the tool will
not object.

### Files this mode writes

| Path | What it is | When |
|---|---|---|
| `~/.config/systemd/user/gewu-agent-<name>.service` | the unit that runs one agent | at every start of a systemd-mode agent; removed when the agent leaves the mode |
| `<config dir>/agents/<name>/supervision.toml` | the mode record, and the run the unit points at | on a mode switch and on every start |
| `<state dir>/<name>/<run id>/` | the run's plan, log and transcript, exactly as in the default mode | on every start |

The unit file is mode 0644 (it is not a secret) and the record is mode 0600, in the
agent's own 0700 directory.
