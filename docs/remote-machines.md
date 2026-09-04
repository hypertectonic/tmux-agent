# Remote machines

tmux-agent federates selected machines over SSH. It does not expose an
application TCP port and does not define a separate remote trust model.

## Requirements

- The same tmux-agent application and protocol version on every machine.
- Working non-interactive SSH authentication from the central machine.
- Accepted SSH host keys.
- An absolute path to the remote tmux-agent binary.

Tailscale, a private LAN, a jump host, or another route can make SSH reachable.
tmux-agent only invokes SSH and leaves network policy to the user.

## Configuration

Add a structured machine to `~/.config/tmux-agent/config.toml`:

```toml
[[machine]]
name = "build-host"
host = "build-host.example.ts.net"
ssh_user = "agent"
binary = "/home/agent/.local/bin/tmux-agent"
```

Set `auto_connect = false` when the machine should remain configured for
interactive operations but should not start a background collector.

## Setup order

1. From a user-controlled shell, update each remote explicitly with ordinary
   SSH. tmux-agent does not issue this command or orchestrate remote updates:

   ```sh
   ssh -T agent@build-host.example.ts.net \
     '/home/agent/.local/bin/tmux-agent update'
   ```
2. Inspect the remote managed versions, then verify the selected binary:

   ```sh
   ssh -T agent@build-host.example.ts.net \
     '/home/agent/.local/bin/tmux-agent versions'
   ssh -T agent@build-host.example.ts.net \
     '/home/agent/.local/bin/tmux-agent --version'
   ```

3. Update the central machine.
4. Restart the central daemon:

   ```sh
   tmux-agent daemon restart
   ```

5. Verify peers:

   ```sh
   tmux-agent doctor
   tmux-agent list
   ```

Federation protocol changes require all machines to be updated together.
Protocol mismatches are rejected with both the received and required version.

If a remote update needs to be reversed, choose a version from that host's
`versions` output and explicitly run:

```sh
ssh -T agent@build-host.example.ts.net \
  '/home/agent/.local/bin/tmux-agent rollback <version>'
```

These are ordinary SSH commands initiated and authorized by the user. Remote
configuration grants federation read access only; it never grants tmux-agent
authority to run lifecycle commands on another machine.

## What crosses SSH

Normal federation snapshots contain derived metadata such as provider, state,
attention, title, location, numeric lifecycle timestamps, goal state without
its objective, and process relationships reduced to identifiers.
Local last-used focus timestamps are not part of federation snapshots.

Snapshots do not contain:

- captured pane contents or terminal screen buffers
- prompts, responses, or reasoning
- raw process command lines
- goal objectives
- rollout events or child transcripts

Opening a remote Codex child starts a separate interactive SSH command for the
read-only viewer. That explicit action can display assistant messages and tool
output. The content does not enter the normal federation snapshot or central
persistence.

## Remote focus

For remote tmux, tmux-agent finds the session currently attached through each
local SSH or Mosh pane. Selecting an agent in a hidden remote window focuses
its outer transport even when the active remote window contains only a shell
or editor. Multiple sessions on one host resolve independently. Two local
transports attached to the same selected session are ambiguous.

This discovery needs `lsof` on both machines. The remote collector reads the
owning tmux server's live clients and follows each client's ancestry to its SSH
or Mosh server socket. Local Mosh clients expose their numeric server endpoint
after the bootstrap SSH connection ends, so discovery survives Mosh roaming.
Named tmux servers are supported when the configured remote collector targets
that server. No launch wrapper, registry, extra service, or dotfile edit is
needed.

Custom Mosh clients are recognized when the original `--client` option names
the exact running executable and its process title carries a numeric server
endpoint. Custom executable paths containing whitespace are not supported.

Session switches update the association on the next scan. Disconnect removes
it; reconnect discovers a new client. Socket and process updates can take about
one second. Dead panes and tmux-agent UI panes are excluded. Stale host/session
markers cannot override a known live association, and no arbitrary same-host
pane is selected when the selected session has no matching transport.

This selects only the outer pane. It does not change or verify the inner tmux
window or pane. The UI reports partial focus and keeps a popup open; the CLI
prints the same notice to stderr and exits successfully. Activating a completion
or pending goal achievement still acknowledges it, but partial focus does not
change last-used ordering. A missing target still permits the UI's existing
acknowledgement action; ambiguous transports remain errors.

Completion visibility requires both the remote agent pane and its uniquely
resolved local transport to be visible. Hidden remote agents remain hidden when
a transport is selected. A transport label takes precedence over a remote pane
label only for a unique association:

```tmux
set -pt:. @pane_label 'testing env'
```

If client ancestry or sockets cannot be inspected, explicitly bind the local
pane to the selected remote session:

```sh
tmux-agent remote bind build-host agents --pane %42
tmux-agent remote bindings
tmux-agent remote unbind --pane %42
```

The binding uses pane-local `@tmux_agent_remote_host` and
`@tmux_agent_remote_session` options and disappears with that pane. Omitting
`--pane` uses the current local `$TMUX_PANE`. Bind validates the configured
remote name and never chooses a pane for you. Update the binding yourself after
switching sessions when automatic inspection is unavailable.

Automatic association may be unavailable for translated server addresses,
SSH proxy or multiplex setups that obscure the connection, or remote tmux
clients running inside another tmux server. When every attached client's
transport is inspectable, no local match fails closed even when old markers
exist. Restore direct endpoint visibility for these cases. If some attached
clients cannot be inspected, an exact explicit binding can identify an
uninspectable client even when other known clients have no local match. You
must update that binding after switching sessions.

Ordinary remote terminals retain their existing SSH connection matching and
unique unmarked Mosh destination/title matching. Older compatibility records
without attachment metadata retain legacy binding/title recovery; live peers
must all use federation protocol 4.

## Custom SSH commands

For unusual SSH setups, an explicit command vector remains available:

```toml
[[remote]]
name = "build-host"
command = ["ssh", "-T", "build-host", "tmux-agent", "watch", "--jsonl", "--local-only"]
```

The structured `[[machine]]` form is preferred because it also supports
diagnostics and remote Codex child viewing.
