tmux-agent v0.2.0 is the first public release of the local-first activity view
for coding agents running in tmux, ordinary terminals, and explicitly
configured SSH machines.

## Highlights

- Discover Codex, Claude, OpenCode, and Grok sessions in tmux and ordinary
  terminals.
- Show working, needs-input, idle, done, and evidence-limited unknown states.
- Display Codex goal progress and nested subagents without forwarding goal
  objectives or rollout content in federation snapshots.
- Focus the exact tmux pane from the terminal UI.
- Federate selected SSH machines without adding an application TCP listener or
  shared application token.
- Install and update through TPM with checksum and binary-version verification.

## Downloads

Signed build provenance is available for all release assets. Archives are
provided for macOS Apple Silicon, macOS Intel, Linux x86-64, and Linux ARM64.
Verify downloaded archives with `SHA256SUMS` before installation.

## Documentation

See the
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.2.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.2.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.2.0/SECURITY.md).
