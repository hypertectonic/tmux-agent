tmux-agent v0.8.1 makes remote completion and focus behavior more reliable for
SSH, Mosh, restored and detached nested tmux sessions, and ordinary remote
terminals.

## Highlights

- A completion behind a hidden, uniquely resolved SSH or Mosh transport now
  stays unseen until its local transport becomes visible.
- Restored nested tmux sessions can repair one stale Mosh session binding during
  focus when the host and session-title evidence identify exactly one pane.
- Ordinary remote terminal rows can focus through a unique matching Mosh
  transport, with existing SSH behavior preserved.
- A detached nested tmux session can recover through one unique idle Mosh
  shell, with the attached title verified before binding markers are written.
- An active nested tmux session can focus across local windows even when
  another session on the same remote host is already bound.
- Missing or ambiguous transport evidence still fails closed without writing a
  binding.

## Compatibility

- No configuration or federation protocol migration is required.
- Existing local, direct SSH, and explicit remote-binding workflows keep their
  current behavior.
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
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.8.1/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.8.1/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.8.1/SECURITY.md).
