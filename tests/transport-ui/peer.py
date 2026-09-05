#!/usr/bin/python3
"""Old-peer adapter: remove one capability, never fabricate scanner records."""

import json
import os
from pathlib import Path
import subprocess
import sys

command = ["/opt/tmux-agent", "--config", "/tmp/transport-ui/inner.toml", *sys.argv[1:]]
if sys.argv[1:2] != ["watch"]:
    os.execv(command[0], command)

child = subprocess.Popen(command, stdout=subprocess.PIPE, text=True)
try:
    for line in child.stdout:
        snapshot = json.loads(line)
        if Path("/tmp/transport-ui/old-peer").exists():
            snapshot["capabilities"] = [
                capability for capability in snapshot.get("capabilities", [])
                if capability != "remote_tmux_focus_v1"
            ]
        print(json.dumps(snapshot), flush=True)
    raise SystemExit(child.wait(timeout=5))
finally:
    child.terminate()
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=5)
