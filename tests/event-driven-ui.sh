#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
command -v tmux >/dev/null 2>&1 || {
    printf '%s\n' 'tmux is required for event-driven UI tests' >&2
    exit 1
}
cargo build --locked

test_root=$(mktemp -d "/tmp/tmux-agent-event-ui-test.XXXXXX")
socket_name="tmux-agent-event-ui-$$"
tmux_tmp="$test_root/tmux"
tmux_test=(env TMUX_TMPDIR="$tmux_tmp" tmux -L "$socket_name")
binary="$root/target/debug/tmux-agent"
config="$test_root/config.toml"
fixture_pid="$test_root/agent.pid"
control_pid=

run_isolated() {
    env \
        HOME="$test_root/home" \
        PATH="$test_root/bin:$PATH" \
        TMUX_TMPDIR="$tmux_tmp" \
        TMUX_AGENT_FIXTURE_PID_FILE="$fixture_pid" \
        XDG_RUNTIME_DIR="$test_root/runtime" \
        XDG_STATE_HOME="$test_root/state" \
        "$binary" --config "$config" "$@"
}

cleanup() {
    exit_code=$?
    run_isolated daemon stop >/dev/null 2>&1 || true
    if [[ -n $control_pid ]]; then
        kill "$control_pid" 2>/dev/null || true
        wait "$control_pid" 2>/dev/null || true
    fi
    "${tmux_test[@]}" kill-server 2>/dev/null || true
    if ((exit_code != 0)); then
        for log in "$test_root"/state/tmux-agent/*.log; do
            if [[ -f $log ]]; then
                printf 'event-driven UI daemon log: %s\n' "$log" >&2
                sed -n '1,160p' "$log" >&2
            fi
        done
    fi
    rm -rf -- "$test_root"
    return "$exit_code"
}
trap cleanup EXIT

mkdir -p "$test_root/bin" "$test_root/home" "$test_root/runtime" "$test_root/state" "$tmux_tmp"
cat >"$test_root/bin/ps" <<'EOF'
#!/bin/sh
set -eu
output=$(/bin/ps "$@")
pid_file=${TMUX_AGENT_FIXTURE_PID_FILE:?}
if [ -s "$pid_file" ]; then
    pid=$(sed -n '1p' "$pid_file")
    printf '%s\n' "$output" | awk -v pid="$pid" \
        '$2 == pid { printf "%s %s %s %s %s %s %s /usr/bin/codex\n", $1, $2, $3, $4, $5, $6, $7 }'
fi
EOF
chmod +x "$test_root/bin/ps"
cat >"$config" <<EOF
host_name = "fixture-host"
server_name = "fixture-server"
scan_interval_ms = 100
tmux_args = ["-L", "$socket_name"]
EOF

"${tmux_test[@]}" -f /dev/null new-session -d -s event-ui 'sleep 300'
"${tmux_test[@]}" set-environment -g HOME "$test_root/home"
"${tmux_test[@]}" set-environment -g PATH "$test_root/bin:$PATH"
"${tmux_test[@]}" set-environment -g TMUX_TMPDIR "$tmux_tmp"
"${tmux_test[@]}" set-environment -g TMUX_AGENT_FIXTURE_PID_FILE "$fixture_pid"
"${tmux_test[@]}" set-environment -g XDG_RUNTIME_DIR "$test_root/runtime"
"${tmux_test[@]}" set-environment -g XDG_STATE_HOME "$test_root/state"

mkfifo "$test_root/control.in"
exec 9<>"$test_root/control.in"
"${tmux_test[@]}" -C attach-session -t event-ui \
    <"$test_root/control.in" >"$test_root/control.out" 2>&1 &
control_pid=$!
for _ in {1..50}; do
    attached=$("${tmux_test[@]}" display-message -p -t event-ui '#{session_attached}')
    [[ $attached == 1 ]] && break
    sleep 0.1
done
if [[ $attached != 1 ]]; then
    printf '%s\n' 'isolated tmux client did not attach' >&2
    exit 1
fi

agent_pane=$(
    "${tmux_test[@]}" split-window -d -P -F '#{pane_id}' \
        'sleep 300'
)
"${tmux_test[@]}" select-pane -t "$agent_pane" -T fixture-task
agent_pid=$("${tmux_test[@]}" display-message -p -t "$agent_pane" '#{pane_pid}')
run_isolated daemon start >"$test_root/daemon-start.out" 2>"$test_root/daemon-start.err" || true
status=
for _ in {1..100}; do
    status=$(run_isolated daemon status)
    [[ $status == running:* ]] && break
    sleep 0.1
done
if [[ $status != running:* ]]; then
    printf 'isolated daemon did not become ready: %s\n' "$status" >&2
    exit 1
fi

ui_pane=$("${tmux_test[@]}" split-window -d -P -F '#{pane_id}' 'sleep 300')
"${tmux_test[@]}" set-option -pt "$ui_pane" remain-on-exit on
"${tmux_test[@]}" respawn-pane -k -t "$ui_pane" "$binary --config $config ui"
for _ in {1..50}; do
    marker=$("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{@tmux_agent_ui}')
    [[ $marker == 1 ]] && break
    sleep 0.1
done
if [[ $marker != 1 ]]; then
    printf '%s\n' 'UI marker was not published' >&2
    exit 1
fi
if [[ $("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{pane_dead}') != 0 ]]; then
    printf '%s\n' 'UI exited before the topology change' >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$ui_pane" >&2
    exit 1
fi
if "${tmux_test[@]}" capture-pane -p -t "$ui_pane" | grep -F 'CODEX' >/dev/null; then
    printf '%s\n' 'synthetic Codex row must not exist before the topology change' >&2
    exit 1
fi
sleep 0.5

printf '%s\n' "$agent_pid" >"$fixture_pid"

discovered=0
for _ in {1..50}; do
    if run_isolated list --json --local-only | grep -F '"agent": "Codex"' >/dev/null; then
        discovered=1
        break
    fi
    sleep 0.1
done
if [[ $discovered != 1 ]]; then
    printf '%s\n' 'daemon did not publish the synthetic Codex row' >&2
    run_isolated list --json --local-only >&2
    exit 1
fi

rendered=0
for _ in {1..50}; do
    if [[ $("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{pane_dead}') == 1 ]]; then
        break
    fi
    if "${tmux_test[@]}" capture-pane -p -t "$ui_pane" | grep -F 'CODEX' >/dev/null; then
        rendered=1
        break
    fi
    sleep 0.1
done

if [[ $("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{pane_dead}') == 1 ]]; then
    printf '%s\n' 'event-driven UI exited after a topology-changing snapshot' >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$ui_pane" >&2
    exit 1
fi
if [[ $rendered != 1 ]]; then
    visibility=$(
        "${tmux_test[@]}" display-message -p -t "$ui_pane" \
            '#{window_active}|#{session_attached}'
    )
    printf 'event-driven UI did not render the published row; visibility=%s\n' \
        "$visibility" >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$ui_pane" >&2
    exit 1
fi

printf '%s\n' 'event-driven UI tests passed'
