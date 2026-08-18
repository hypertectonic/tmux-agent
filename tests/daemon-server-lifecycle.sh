#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
command -v tmux >/dev/null 2>&1 || {
    printf '%s\n' 'tmux is required for daemon lifecycle tests' >&2
    exit 1
}
real_tmux=$(command -v tmux)
cargo build --locked

test_root=$(mktemp -d "/tmp/tmux-agent-daemon-lifecycle-test.XXXXXX")
socket_name="tmux-agent-daemon-lifecycle-$$"
tmux_tmp="$test_root/tmux"
tmux_test=(env TMUX_TMPDIR="$tmux_tmp" "$real_tmux" -L "$socket_name")
binary="$root/target/debug/tmux-agent"
config="$test_root/config.toml"
daemon_pid=
collector_pid=

daemon_command=(
    env
    HOME="$test_root/home"
    PATH="$test_root/bin:$PATH"
    TMUX_TMPDIR="$tmux_tmp"
    TMUX_AGENT_LIFECYCLE_TEST_ROOT="$test_root"
    XDG_RUNTIME_DIR="$test_root/runtime"
    XDG_STATE_HOME="$test_root/state"
    "$binary" --config "$config" daemon run
)

run_isolated() {
    env \
        HOME="$test_root/home" \
        PATH="$test_root/bin:$PATH" \
        TMUX_TMPDIR="$tmux_tmp" \
        TMUX_AGENT_LIFECYCLE_TEST_ROOT="$test_root" \
        XDG_RUNTIME_DIR="$test_root/runtime" \
        XDG_STATE_HOME="$test_root/state" \
        "$binary" --config "$config" "$@"
}

cleanup() {
    exit_code=$?
    if [[ -n $daemon_pid ]] && kill -0 "$daemon_pid" 2>/dev/null; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    if [[ -n $collector_pid ]] && kill -0 "$collector_pid" 2>/dev/null; then
        kill "$collector_pid" 2>/dev/null || true
    fi
    "${tmux_test[@]}" kill-server 2>/dev/null || true
    if ((exit_code != 0)); then
        printf '%s\n' 'daemon lifecycle log:' >&2
        sed -n '1,160p' "$test_root/daemon.log" >&2 2>/dev/null || true
    fi
    rm -rf -- "$test_root"
    return "$exit_code"
}
trap cleanup EXIT

mkdir -p "$test_root/bin" "$test_root/home" "$test_root/runtime" \
    "$test_root/state" "$tmux_tmp"

cat >"$test_root/bin/ps" <<'EOF'
#!/bin/sh
set -eu
test_root=${TMUX_AGENT_LIFECYCLE_TEST_ROOT:?}
if [ -e "$test_root/missing-server-observed" ]; then
    : >"$test_root/ps-after-missing-server"
fi
printf '%s\n' "$*" >>"$test_root/ps-calls"
exit 0
EOF
chmod +x "$test_root/bin/ps"

cat >"$test_root/bin/tmux" <<EOF
#!/bin/sh
set -u
test_root=\${TMUX_AGENT_LIFECYCLE_TEST_ROOT:?}
stdout="\$test_root/tmux-stdout.\$$"
stderr="\$test_root/tmux-stderr.\$$"
is_list_panes=0
for argument in "\$@"; do
    if [ "\$argument" = list-panes ]; then
        is_list_panes=1
        break
    fi
done
if [ "\$is_list_panes" -eq 1 ] && [ -e "\$test_root/force-startup-race" ] &&
    [ ! -e "\$test_root/startup-server-killed" ]; then
    : >"\$test_root/startup-server-killed"
    env TMUX_TMPDIR="$tmux_tmp" "$real_tmux" -L "$socket_name" kill-server \
        >/dev/null 2>&1 || true
fi
"$real_tmux" "\$@" >"\$stdout" 2>"\$stderr"
status=\$?
if [ "\$status" -ne 0 ] && [ "\$is_list_panes" -eq 1 ] &&
    grep -Eq 'server exited unexpectedly|no server running|error connecting to|no sessions' "\$stderr"; then
    : >"\$test_root/missing-server-observed"
    printf '%s\n' missing >>"\$test_root/missing-server-results"
fi
cat "\$stdout"
cat "\$stderr" >&2
rm -f -- "\$stdout" "\$stderr"
exit "\$status"
EOF
chmod +x "$test_root/bin/tmux"

cat >"$test_root/bin/collector" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$$" >"${TMUX_AGENT_LIFECYCLE_TEST_ROOT:?}/collector.pid"
exec /usr/bin/tail -f /dev/null
EOF
chmod +x "$test_root/bin/collector"

cat >"$config" <<EOF
host_name = "fixture-host"
server_name = "fixture-server"
scan_interval_ms = 100
tmux_args = ["-L", "$socket_name"]

[[remote]]
name = "fixture-remote"
command = ["$test_root/bin/collector"]
EOF

"${tmux_test[@]}" -f /dev/null new-session -d -s lifecycle 'sleep 300'
socket=$(run_isolated paths | awk '$1 == "socket" { print $2 }')

"${daemon_command[@]}" >"$test_root/daemon.log" 2>&1 &
daemon_pid=$!

for _ in {1..50}; do
    [[ -S $socket && -s $test_root/collector.pid && -s $test_root/ps-calls ]] && break
    sleep 0.1
done
if [[ ! -S $socket || ! -s $test_root/collector.pid || ! -s $test_root/ps-calls ]]; then
    printf '%s\n' 'daemon did not become ready with its remote collector' >&2
    exit 1
fi
collector_pid=$(<"$test_root/collector.pid")

"${tmux_test[@]}" kill-server

daemon_stopped=0
for _ in {1..50}; do
    state=$({ /bin/ps -o stat= -p "$daemon_pid" 2>/dev/null || true; } | tr -d '[:space:]')
    if [[ -z $state || $state == Z* ]]; then
        daemon_stopped=1
        break
    fi
    sleep 0.1
done
if [[ $daemon_stopped != 1 ]]; then
    printf '%s\n' 'daemon did not exit after its tmux server disappeared' >&2
    exit 1
fi
wait "$daemon_pid"
daemon_pid=

if [[ -S $socket ]]; then
    printf '%s\n' 'daemon socket remained after the tmux server disappeared' >&2
    exit 1
fi
if kill -0 "$collector_pid" 2>/dev/null; then
    printf '%s\n' 'remote collector remained after daemon shutdown' >&2
    exit 1
fi
if [[ ! -s $test_root/missing-server-results ]]; then
    printf '%s\n' 'test did not observe the missing tmux server' >&2
    exit 1
fi
missing_count=$(wc -l <"$test_root/missing-server-results" | tr -d ' ')
if [[ $missing_count != 3 ]]; then
    printf 'daemon exited after %s missing-server observations, expected 3\n' \
        "$missing_count" >&2
    exit 1
fi
if [[ -e $test_root/ps-after-missing-server ]]; then
    printf '%s\n' 'global process discovery ran after the tmux server was reported missing' >&2
    exit 1
fi

rm -f -- "$test_root/collector.pid" "$test_root/missing-server-observed" \
    "$test_root/missing-server-results" "$test_root/ps-after-missing-server" \
    "$test_root/ps-calls" "$test_root/startup-server-killed"
collector_pid=
"${tmux_test[@]}" -f /dev/null new-session -d -s startup-race 'sleep 300'
touch "$test_root/force-startup-race"

"${daemon_command[@]}" >"$test_root/daemon.log" 2>&1 &
daemon_pid=$!
daemon_stopped=0
for _ in {1..50}; do
    state=$({ /bin/ps -o stat= -p "$daemon_pid" 2>/dev/null || true; } | tr -d '[:space:]')
    if [[ -z $state || $state == Z* ]]; then
        daemon_stopped=1
        break
    fi
    sleep 0.1
done
if [[ $daemon_stopped != 1 ]]; then
    printf '%s\n' 'daemon did not exit during the tmux startup race' >&2
    exit 1
fi
if wait "$daemon_pid"; then
    daemon_status=0
else
    daemon_status=$?
fi
daemon_pid=
if [[ $daemon_status != 0 ]]; then
    printf 'startup race returned daemon status %s, expected a clean exit\n' \
        "$daemon_status" >&2
    exit 1
fi
if [[ -S $socket ]]; then
    printf '%s\n' 'daemon socket was created during the tmux startup race' >&2
    exit 1
fi
if [[ -e $test_root/collector.pid ]]; then
    printf '%s\n' 'remote collector started during the tmux startup race' >&2
    exit 1
fi
missing_count=$(wc -l <"$test_root/missing-server-results" | tr -d ' ')
if [[ $missing_count != 3 ]]; then
    printf 'startup race exited after %s missing-server observations, expected 3\n' \
        "$missing_count" >&2
    exit 1
fi
if [[ -e $test_root/ps-calls || -e $test_root/ps-after-missing-server ]]; then
    printf '%s\n' 'global process discovery ran during the tmux startup race' >&2
    exit 1
fi

printf '%s\n' 'daemon server lifecycle tests passed'
