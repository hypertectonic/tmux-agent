"""Production scanner + persistent UI through isolated OpenSSH and Mosh."""

import fcntl
import json
import os
from pathlib import Path
import pty
import pwd
import select
import shlex
import shutil
import struct
import subprocess
import sys
import termios
import time

ROOT = Path("/tmp/transport-ui")
BIN = "/opt/tmux-agent"
TITLE = "transport-gate-target"
ENV = dict(os.environ, TERM="xterm-256color", LANG="C.UTF-8")
ENV.pop("TMUX", None)
ENV.pop("TMUX_PANE", None)


def run(args, **kwargs):
    return subprocess.run(args, env=ENV, text=True, capture_output=True,
                          check=True, timeout=10, **kwargs).stdout.strip()


def prepare_login():
    """Root exists only to supply the real OpenSSH permission boundary."""
    for program in ["tmux", "mosh", "mosh-client", "mosh-server", "ssh", "sshd",
                    "ssh-keygen", "lsof", "ps", "runuser"]:
        if shutil.which(program) is None:
            raise RuntimeError(f"container prerequisite missing: {program}")
    ROOT.mkdir(mode=0o755)
    ssh = Path("/home/agent/.ssh")
    ssh.mkdir(mode=0o700)
    run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(ssh / "key")])
    run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(ROOT / "host-key")])
    (ssh / "authorized_keys").write_bytes((ssh / "key.pub").read_bytes())
    public_key = (ROOT / "host-key.pub").read_text().split()
    (ssh / "known_hosts").write_text(f"[127.0.0.1]:2222 {' '.join(public_key[:2])}\n")
    (ssh / "config").write_text(
        "Host fixture-peer\n HostName 127.0.0.1\n Port 2222\n User agent\n"
        " IdentityFile /home/agent/.ssh/key\n IdentitiesOnly yes\n"
        " StrictHostKeyChecking yes\n BatchMode yes\n ConnectTimeout 5\n")
    (ROOT / "sshd.conf").write_text(
        f"Port 2222\nListenAddress 127.0.0.1\nHostKey {ROOT}/host-key\n"
        f"PidFile {ROOT}/sshd.pid\nAuthorizedKeysFile .ssh/authorized_keys\n"
        "PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n"
        "PermitRootLogin no\nAllowUsers agent\nPrintMotd no\nLogLevel ERROR\n")
    user = pwd.getpwnam("agent")
    for path in [ssh, *ssh.iterdir(), ROOT]:
        os.chown(path, user.pw_uid, user.pw_gid)
    for path in ssh.iterdir():
        path.chmod(0o600)
    server = subprocess.Popen(["/usr/sbin/sshd", "-D", "-e", "-f", str(ROOT / "sshd.conf")],
                              stdout=subprocess.DEVNULL)
    try:
        deadline = time.monotonic() + 5
        while True:
            if server.poll() is not None:
                raise RuntimeError(f"sshd exited before listening: status {server.returncode}")
            listener = subprocess.run(
                ["lsof", "-nP", "-a", "-p", str(server.pid), "-iTCP:2222", "-sTCP:LISTEN", "-Fn"],
                capture_output=True, text=True, timeout=1, check=False)
            fields = listener.stdout.splitlines()
            if listener.returncode == 0 and f"p{server.pid}" in fields and "n127.0.0.1:2222" in fields:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("sshd did not listen on 127.0.0.1:2222 within 5 seconds")
            time.sleep(0.05)
        subprocess.run(["runuser", "-u", "agent", "--", "python3", __file__, "--user"],
                       check=True, timeout=210)
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=5)


class Scenario:
    def __init__(self, transport):
        self.transport = transport
        self.clients = []
        self.ui = None
        self.target = None

    def tmux(self, side, *args):
        return run(["tmux", "-S", self.socket(side), *args])

    def socket(self, side):
        return str(ROOT / f"{self.transport.lower()}-{side}.sock")

    def app(self, *args):
        return run([BIN, "--config", str(ROOT / "outer.toml"), *args])

    def pause(self, seconds=0.05):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            for master, _ in self.clients:
                while select.select([master], [], [], 0)[0]:
                    try:
                        if not os.read(master, 65536):
                            break
                    except OSError:
                        break
            time.sleep(0.01)

    def wait(self, label, predicate):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if predicate():
                return
            self.pause()
        detail = self.screen() if self.ui else "UI not started"
        raise AssertionError(f"{self.transport}: timed out: {label}\n{detail}")

    def record(self):
        records = [record for record in json.loads(self.app("list", "--json"))["agents"]
                   if record.get("remote_alias") == "fixture-peer"
                   and record.get("pane_id") == self.target]
        return records[0] if len(records) == 1 else {}

    def locations(self):
        rows = self.tmux("outer", "list-clients", "-F",
                         "#{client_pid}|#{session_name}|#{window_id}|#{pane_id}")
        return {int(fields[0]): fields[1:] for fields in (row.split("|") for row in rows.splitlines())}

    def current_inner(self):
        return self.tmux("inner", "display-message", "-p", "-t", "source:",
                         "#{window_id}|#{pane_id}").split("|")

    def screen(self):
        return self.tmux("outer", "capture-pane", "-p", "-t", self.ui)

    def transport_pane(self):
        command = ["tmux", "-S", self.socket("inner"), "attach-session", "-t", "source"]
        if self.transport == "SSH":
            command = ["ssh", "-tt", "fixture-peer", shlex.join(command)]
        else:
            command = ["mosh", "--no-init", "--predict=never", "fixture-peer", "--", *command]
        return self.tmux("outer", "new-window", "-d", "-t", "transport:", "-P", "-F",
                         "#{pane_id}", shlex.join(command))

    def setup(self):
        for side in ["inner", "outer"]:
            (ROOT / f"{side}.toml").write_text(
                f'host_name = "{side}-fixture"\nscan_interval_ms = 100\n'
                f'tmux_args = ["-S", "{self.socket(side)}"]\n'
                + ('[[machine]]\nname = "fixture-peer"\nhost = "fixture-peer"\n'
                   'ssh_user = "agent"\nbinary = "/opt/fixture/peer.py"\n' if side == "outer" else ""))
        self.tmux("inner", "-f", "/dev/null", "new-session", "-d", "-s", "source", "sleep 180")
        self.tmux("inner", "new-window", "-d", "-t", "source:", "-n", "hidden", "sleep 180")
        self.target = self.tmux("inner", "split-window", "-d", "-t", "source:hidden", "-P", "-F",
                                "#{pane_id}", "bash -c 'exec -a claude sleep 180'")
        self.tmux("inner", "select-pane", "-t", self.target, "-T", f"◐ {TITLE}")
        self.tmux("outer", "-f", "/dev/null", "new-session", "-d", "-s", "ui", "sleep 180")
        self.ui = self.tmux("outer", "split-window", "-h", "-t", "ui:", "-P", "-F",
                            "#{pane_id}", shlex.join([BIN, "--config", str(ROOT / "outer.toml"), "ui"]))
        self.tmux("outer", "new-session", "-d", "-s", "transport", "sleep 180")
        self.transport_target = self.transport_pane()
        self.tmux("outer", "select-pane", "-t", self.ui)
        for _ in range(2):
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 500, 0, 0))
            child = subprocess.Popen(["tmux", "-S", self.socket("outer"), "attach-session", "-t", "ui"],
                                     stdin=slave, stdout=slave, stderr=slave, env=ENV)
            os.close(slave)
            self.clients.append((master, child))
            self.wait("PTY client attached", lambda: child.pid in self.locations())
        self.wait("production scanner attachment identity", lambda:
                  self.record().get("session_connections", {}).get("complete")
                  and len(self.record().get("session_connections", {}).get("clients", [])) == 1)
        self.wait("initial local transport resolved", lambda:
                  self.record().get("focus_target", {}).get("pane_id") == self.transport_target)
        record = self.record()
        assert record["pane_index"] == 1 and not record["visible"], "target must start hidden and non-first"
        assert self.current_inner() != [record["window_id"], self.target]
        if self.transport == "SSH":
            self.check_permission_boundary()
        self.complete_turn()

    def check_permission_boundary(self):
        pid = int(self.tmux("inner", "list-clients", "-F", "#{client_pid}"))
        for _ in range(64):
            command = Path(f"/proc/{pid}/comm").read_text().strip()
            if command.startswith("sshd"):
                try:
                    list(Path(f"/proc/{pid}/fd").iterdir())
                except PermissionError:
                    print("PASS privileged sshd FD table unreadable by production scanner user", flush=True)
                    return
                raise AssertionError("sshd login FD table unexpectedly readable")
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            pid = int(fields[1])
        raise AssertionError("real SSH login ancestry was not found")

    def complete_turn(self):
        self.tmux("inner", "select-pane", "-t", self.target, "-T", f"◐ {TITLE}")
        self.wait("working scan", lambda: self.record().get("state") == "working")
        self.tmux("inner", "select-pane", "-t", self.target, "-T", f"✳ {TITLE}")
        self.wait("unread completion", lambda: self.record().get("attention") == "done")
        self.wait("persistent UI row", lambda: TITLE in self.screen())

    def activate(self, notice, acknowledge, inner_exact):
        initiating = self.clients[0][1].pid
        spectator = self.clients[1][1].pid
        before = self.locations()
        expected = self.tmux("outer", "display-message", "-p", "-t", self.transport_target,
                             "#{session_name}|#{window_id}|#{pane_id}").split("|")
        remote_before = self.current_inner()
        os.write(self.clients[0][0], f"/{TITLE}".encode())
        self.wait("typed UI filter", lambda: f"search {TITLE}" in self.screen())
        os.write(self.clients[0][0], b"\r")
        observed = set()
        def outcome_visible():
            screen = self.screen()
            observed.update(line.strip() for line in screen.splitlines()
                            if "focused " in line or "not confirmed" in line)
            return notice in screen
        try:
            self.wait(f"UI outcome {notice}", outcome_visible)
        except AssertionError as error:
            raise AssertionError(f"{error}\nobserved notices: {sorted(observed)}") from error
        after = self.locations()
        assert after[initiating] == expected, "initiating client did not reach exact outer pane"
        assert after[spectator] == before[spectator], "spectator moved during remote activation"
        if inner_exact:
            assert self.current_inner() == [self.record()["window_id"], self.target], "wrong inner pane"
        else:
            assert self.current_inner() == remote_before, "non-exact outcome mutated inner selection"
        if acknowledge:
            self.wait("completion acknowledged", lambda: self.record().get("seen") is True)
        else:
            self.pause(0.3)
            assert self.record().get("attention") == "done" and not self.record()["seen"], "failed control acknowledged completion"
        print(f"PASS {self.transport} {notice}: initiator exact, spectator unchanged, acknowledgement={acknowledge}", flush=True)

    def reset_ui(self, clear_search=False):
        name = self.tmux("outer", "list-clients", "-F", "#{client_pid}|#{client_name}")
        initiating = next(row.split("|", 1)[1] for row in name.splitlines()
                          if row.split("|", 1)[0] == str(self.clients[0][1].pid))
        self.tmux("outer", "switch-client", "-c", initiating, "-t", "ui:")
        self.tmux("outer", "select-pane", "-t", self.ui)
        if clear_search:
            os.write(self.clients[0][0], b"\x1b")
            self.wait("failed search cleared", lambda: not any(
                line.startswith(" search ") for line in self.screen().splitlines()))
        self.pause(0.1)
        self.tmux("inner", "select-window", "-t", "source:0")

    def exercise(self):
        self.setup()
        self.activate("focused transport:1.0/fixture-peer", True, True)
        self.reset_ui()
        # Reconnect to a new local pane. Keep obsolete markers on the old pane.
        self.tmux("outer", "set-option", "-pt", self.transport_target, "@tmux_agent_remote_host", "fixture-peer")
        self.tmux("outer", "set-option", "-pt", self.transport_target, "@tmux_agent_remote_session", "source")
        self.tmux("outer", "set-option", "-wt", self.transport_target, "remain-on-exit", "on")
        stale_pane = self.transport_target
        remote_client = self.tmux("inner", "list-clients", "-F", "#{client_name}")
        self.tmux("inner", "detach-client", "-t", remote_client)
        self.wait("transport disconnected", lambda: not self.record().get("session_connections", {}).get("clients"))
        assert self.tmux("outer", "show-option", "-pqv", "-t", stale_pane,
                         "@tmux_agent_remote_session") == "source", "stale marker must survive disconnect"
        self.transport_target = self.transport_pane()
        self.wait("replacement transport resolved", lambda: self.record().get("focus_target", {}).get("pane_id") == self.transport_target)
        self.complete_turn()
        self.activate("focused transport:2.0/fixture-peer", True, True)
        print(f"PASS {self.transport} reconnect ignores stale binding", flush=True)
        self.reset_ui()
        # This is a real supported-control rejection, not a fabricated response.
        client = self.tmux("inner", "list-clients", "-F", "#{client_name}")
        self.tmux("inner", "refresh-client", "-f", "active-pane", "-t", client)
        self.complete_turn()
        self.activate("active-pane", False, False)
        self.tmux("inner", "refresh-client", "-f", "!active-pane", "-t", client)
        self.reset_ui(clear_search=True)
        (ROOT / "old-peer").touch()
        self.complete_turn()
        self.wait("old-peer capability omission", lambda: all(
            "remote_tmux_focus_v1" not in peer.get("capabilities", [])
            for peer in json.loads(self.app("list", "--json"))["peers"]))
        self.activate("peer does not advertise remote_tmux_focus_v1", True, False)

    def close(self):
        for side in ["outer", "inner"]:
            try:
                run([BIN, "--config", str(ROOT / f"{side}.toml"), "daemon", "stop"])
            except (subprocess.SubprocessError, OSError):
                pass
            try:
                self.tmux(side, "kill-server")
            except (subprocess.SubprocessError, OSError):
                pass
        for master, child in self.clients:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            os.close(master)
        (ROOT / "old-peer").unlink(missing_ok=True)


if __name__ == "__main__":
    if os.geteuid() == 0:
        prepare_login()
    else:
        assert sys.argv[1:] == ["--user"]
        for transport in ["SSH", "Mosh"]:
            scenario = Scenario(transport)
            try:
                scenario.exercise()
            finally:
                scenario.close()
        print("PASS production SSH/Mosh persistent UI cases", flush=True)
