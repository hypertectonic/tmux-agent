#!/usr/bin/env python3
"""Disposable scanner/focus contract with synthetic Claude registry and events.

This tests production wiring, not an authenticated Claude model run.
"""
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time


def provider():
    root = Path(os.environ["CLAUDE_CONFIG_DIR"])
    registry = root / "sessions"
    registry.mkdir(parents=True)
    start = subprocess.check_output(
        ["ps", "-o", "lstart=", "-p", str(os.getpid())],
        env={**os.environ, "LC_ALL": "C", "TZ": "UTC"}, text=True,
    ).strip()
    cwd = str(Path.cwd())
    (registry / f"{os.getpid()}.json").write_text(json.dumps({
        "pid": os.getpid(), "sessionId": "fixture-session", "cwd": cwd,
        "procStart": start,
    }))
    children = root / "projects" / re.sub(r"[^A-Za-z0-9]", "-", cwd) / "fixture-session" / "subagents"
    children.mkdir(parents=True)
    for name, done in [("one", False), ("two", True)]:
        event = {
            "sessionId": "fixture-session", "agentId": name, "isSidechain": True,
            "type": "user", "toolEndsTurn": done,
            "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "message": {"content": "Synthetic private body must not be forwarded"},
        }
        (children / f"agent-{name}.jsonl").write_text(json.dumps(event) + "\n")
        (children / f"agent-{name}.meta.json").write_text(json.dumps({"agentType": "reviewer"}))
    print("Synthetic Claude process", flush=True)
    time.sleep(90)


def main():
    if not shutil.which("tmux"):
        raise RuntimeError("tmux is required for Claude child contract test")
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="tmux-agent-claude-children-") as directory:
        root = Path(directory)
        socket = str(root / "tmux.sock")
        config = root / "config.toml"
        config.write_text(f'host_name = "fixture-host"\ntmux_args = ["-S", {json.dumps(socket)}]\n')
        env = {**os.environ, "CLAUDE_CONFIG_DIR": str(root / "claude-home"),
               "CODEX_HOME": str(root / "codex-home"), "XDG_RUNTIME_DIR": str(root / "runtime"),
               "XDG_STATE_HOME": str(root / "state"), "TMUX": "", "TMUX_PANE": ""}
        (root / "runtime").mkdir(mode=0o700)
        fixture = root / "claude"
        shutil.copyfile(__file__, fixture)
        fixture.chmod(0o700)

        def tmux(*args):
            return subprocess.check_output(["tmux", "-S", socket, *args], env=env, text=True).strip()

        def agent(*args):
            return subprocess.check_output([str(binary), "--config", str(config), *args], env=env, text=True)

        try:
            pane = tmux("-f", "/dev/null", "new-session", "-d", "-P", "-F", "#{pane_id}",
                        "-s", "fixture", "-c", str(root), f"exec {fixture} --provider")
            deadline = time.monotonic() + 10
            while True:
                snapshot = json.loads(agent("scan", "--json"))
                children = [row for row in snapshot["agents"] if row.get("subagent")]
                if len(children) == 2:
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError("synthetic Claude children were not discovered")
                time.sleep(0.2)
            assert {row["attention"] for row in children} == {"working", "done"}
            assert all(row["subagent"]["name"] == "reviewer" for row in children)
            assert "Synthetic private body" not in json.dumps(snapshot)
            tmux("new-window", "-t", "fixture:", "-n", "other")
            child = next(row for row in children if row["state"] == "working")
            agent("focus", child["id"])
            assert tmux("display-message", "-p", "-t", "fixture:", "#{pane_id}") == pane
            print("PASS: production scanner discovers duplicate names, retains completion, excludes content, and focuses parent")
        finally:
            subprocess.run([str(binary), "--config", str(config), "daemon", "stop"],
                           env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            subprocess.run(["tmux", "-S", socket, "kill-server"], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)


if __name__ == "__main__":
    if sys.argv[1:] == ["--provider"]:
        provider()
    else:
        main()
