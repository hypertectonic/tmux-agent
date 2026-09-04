"""Start a real loopback Mosh transport without exposing its bootstrap key."""

import os
import re
import subprocess
import sys

os.environ.pop("TMUX", None)
os.environ.pop("TMUX_PANE", None)
bootstrap = subprocess.run(
    [
        "mosh-server", "new", "-s", "-i", "127.0.0.1", "--",
        "tmux", "-L", sys.argv[1], "attach-session", "-t", sys.argv[2],
    ],
    capture_output=True,
    text=True,
    check=True,
)
connection = re.search(r"MOSH CONNECT (\d+) (\S+)", bootstrap.stdout)
if connection is None:
    raise SystemExit("Mosh bootstrap did not produce a connection")
os.environ["MOSH_KEY"] = connection[2]
os.execvp(
    "mosh-client",
    ["mosh-client", "-#", "--no-init remote-host |", "127.0.0.1", connection[1]],
)
