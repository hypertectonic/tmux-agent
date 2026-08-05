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

1. Log in to each remote machine and update it locally. tmux-agent does not
   orchestrate remote updates:

   ```sh
   tmux-agent update
   ```
2. Verify each remote directly:

   ```sh
   ssh agent@build-host.example.ts.net \
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

## What crosses SSH

Normal federation snapshots contain derived metadata such as provider, state,
attention, title, location, numeric lifecycle timestamps, goal state without
its objective, and process relationships reduced to identifiers.

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

When a remote session was reached through an SSH process inside local tmux,
tmux-agent matches the two endpoints of that established connection and can
focus the local pane carrying it.

An integration that provides mirror panes can set:

```tmux
set -pt:. @tmux_agent_remote_host 'build-host'
set -pt:. @tmux_agent_remote_session 'agents'
```

Only the public `@tmux_agent_remote_host` and
`@tmux_agent_remote_session` marker names are recognized.

Set both markers on a local pane that attaches to a tmux session on the remote
machine. tmux-agent does not use a matching title alone for a remote tmux
agent because another SSH pane can have the same title.

If no unique local SSH or mirror pane can be resolved, tmux-agent reports the
ambiguity or missing focus target instead of guessing.

A local transport pane can also provide the label shown beside its remote
agent:

```tmux
set -pt:. @pane_label 'testing env'
```

The local label takes precedence over a remote pane label only when the
transport is resolved uniquely. An ambiguous or unrelated pane does not
contribute a label.

## Custom SSH commands

For unusual SSH setups, an explicit command vector remains available:

```toml
[[remote]]
name = "build-host"
command = ["ssh", "-T", "build-host", "tmux-agent", "watch", "--jsonl", "--local-only"]
```

The structured `[[machine]]` form is preferred because it also supports
diagnostics and remote Codex child viewing.
