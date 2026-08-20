tmux-agent v0.8.0 reduces scanner overhead in large tmux setups, shuts down
daemons whose tmux server has disappeared, and adds explicit focus bindings
for nested remote tmux sessions.

## Highlights

- Scanner work now batches pane captures, caches macOS terminal resolution,
  and refreshes the global process inventory at most once per second.
- Panes in displayed windows retain the normal scan cadence. Hidden-window
  screens are reused for one second and refresh immediately when their process
  changes.
- In a 20-second multi-session comparison, `capture-pane`, `display-message`,
  and wrapper command traffic fell by about 68 percent.
- A daemon exits after three consecutive missing-server scans, removes its
  runtime socket, and terminates its remote collectors instead of continuing
  process discovery indefinitely.
- `tmux-agent remote bind` maps an SSH or Mosh transport pane to a specific
  nested remote tmux session so focus reaches the intended inner pane.

## Compatibility

- No configuration or federation protocol migration is required.
- Nested remote focus bindings are optional. Existing local and direct SSH
  focus paths keep their current behavior.
- Hidden windows may take up to about one second to reflect screen-only state
  changes. Displayed windows remain on the normal scan cadence.
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
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.8.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.8.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.8.0/SECURITY.md).
