tmux-agent v0.11.1 fixes stale activity states when many agent panes need
capturing.

## Fixed

Pane captures now run in bounded batches, avoiding tmux command-size failures
that could leave idle agents marked as working. Successful captures remain
mapped to their panes even when another pane disappears, and fresh idle
observations can clear the previous working state after capture recovery.

## Updating

```sh
tmux-agent update
tmux-agent versions
tmux-agent --version
```

See the [installation guide](https://github.com/hypertectonic/tmux-agent/blob/v0.11.1/docs/installation.md)
and [remote-machine guide](https://github.com/hypertectonic/tmux-agent/blob/v0.11.1/docs/remote-machines.md).
