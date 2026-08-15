tmux-agent v0.6.0 makes large session lists quicker to navigate and keeps
recently used idle sessions within reach.

## Highlights

- Press `1` through `9` or `0` to activate one of the first ten top-level
  sessions. The matching keycap appears beside each provider badge.
- Numeric selection is sent to the other persistent tmux-agent UIs on the same
  tmux server, so their selected row stays aligned after activation.
- Idle sessions sort by the newer of their last successful tmux-agent focus and
  their latest state change. The existing attention buckets and subagent
  hierarchy keep their previous order.
- The local daemon distributes last-used ordering to its connected UIs without
  serializing, persisting, or federating the timestamps.
- The README banner now matches the current header, row layout, shortcut
  keycaps, peer status, version, and footer controls.

## Compatibility

- No configuration or federation protocol migration is required.
- Last-used ordering resets when the local daemon restarts. Manual tmux
  switching does not update it.
- tmux-agent no longer reads or writes the legacy `@tmux_agent_host` and
  `@tmux_agent_host_color` presentation options. It leaves existing values
  untouched. Integrations that render host badges should own their detection,
  colors, and pane options.
- Pane titles, `@pane_label`, remote routing markers, SSH transport discovery,
  aliases, and focus behavior remain unchanged.
- Existing standalone and TPM installations can update through the normal
  verified lifecycle.

## Updating

For standalone and managed TPM installations:

```sh
tmux-agent update
tmux-agent versions
```

TPM's `prefix + U` still updates the plugin checkout. It does not replace the
packaged `tmux-agent update` command. Updates on SSH machines remain ordinary
commands explicitly initiated by the user on each machine.

## Downloads

Signed build provenance is available for all release assets. Archives are
provided for macOS Apple Silicon, macOS Intel, Linux x86-64, and Linux ARM64.
Verify downloaded archives with `SHA256SUMS` before installation.

## Documentation

See the
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.6.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.6.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.6.0/SECURITY.md).
