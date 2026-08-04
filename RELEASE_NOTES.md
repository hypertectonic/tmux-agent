tmux-agent v0.3.0 adds first-class Pi harness support and improves terminal UI
feedback for the local-first activity view.

## Highlights

- Discover Pi sessions from direct executables and package-manager entrypoints
  in tmux and ordinary terminals.
- Detect Pi activity, permission selectors, and project-trust prompts through a
  project-owned typed detector backed by synthetic behavior fixtures.
- Run Pi through the owned PTY with `tmux-agent pi`, including argument, input,
  resize, signal, job-control, and exit-status forwarding.
- Show Pi with its own provider badge in the terminal UI.
- Restore the standard footer key hints three seconds after action feedback,
  while keeping daemon connection errors visible until recovery.

## Downloads

Signed build provenance is available for all release assets. Archives are
provided for macOS Apple Silicon, macOS Intel, Linux x86-64, and Linux ARM64.
Verify downloaded archives with `SHA256SUMS` before installation.

## Documentation

See the
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.3.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.3.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.3.0/SECURITY.md).
