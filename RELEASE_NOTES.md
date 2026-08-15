tmux-agent v0.5.0 makes persistent sidebars substantially cheaper and adds
search-as-you-type navigation for installations with many agent sessions.

## Highlights

- Press `/` in the UI to filter sessions as you type. Existing single-key
  navigation remains unchanged outside search mode.
- Persistent sidebars now use a daemon watch and redraw only when visible state
  changes. Hidden sidebars retain the latest snapshot without rendering.
- In a real 14-sidebar workload, aggregate UI CPU use fell from 10.15% to
  2.17%, a 78.6% reduction. Aggregate RSS remained effectively flat at about
  58 MiB.
- Successful session activation clears the search, so returning to the sidebar
  restores the full list.
- Grok rows use stable working-directory names instead of changing terminal
  activity titles.
- Persistent UIs reconnect automatically after a daemon restart, and explicit
  refresh replaces the existing watch cleanly.

## Compatibility

- No configuration or federation protocol migration is required.
- Existing standalone and TPM installations can update through the normal
  verified lifecycle.
- A hidden sidebar may take up to two seconds to notice that it became visible.
  This keeps visibility polling inexpensive when many sidebars are open.

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
[installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.5.0/docs/installation.md),
[remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.5.0/docs/remote-machines.md),
and [security policy](https://github.com/hypertectonic/tmux-agent/blob/v0.5.0/SECURITY.md).
