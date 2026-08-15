#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
command -v tmux >/dev/null 2>&1 || {
    printf '%s\n' 'tmux is required for event-driven UI tests' >&2
    exit 1
}
real_tmux=$(command -v tmux)
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
    touch "$test_root/wake-release"
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
    printf '%s\n' "$output" | awk \
        'NR == FNR { pids[$1] = 1; next }
         $2 in pids { printf "%s %s %s %s %s %s %s /usr/bin/codex\n", $1, $2, $3, $4, $5, $6, $7 }' \
        "$pid_file" -
fi
EOF
chmod +x "$test_root/bin/ps"
cat >"$test_root/bin/tmux" <<EOF
#!/bin/sh
set -eu
printf '%s\n' "\$*" >>"$test_root/tmux-calls"
for argument in "\$@"; do
    if [ "\$argument" = "-H" ] && [ -e "$test_root/wake-block" ]; then
        printf '%s\n' "\$$" >>"$test_root/wake-started"
        while [ ! -e "$test_root/wake-release" ]; do
            sleep 0.01
        done
        break
    fi
done
exec "$real_tmux" "\$@"
EOF
chmod +x "$test_root/bin/tmux"
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
"${tmux_test[@]}" resize-window -t event-ui:0 -x 100 -y 80

first_agent_pane=$(
    "${tmux_test[@]}" split-window -d -P -F '#{pane_id}' \
        'sleep 300'
)
"${tmux_test[@]}" select-pane -t "$first_agent_pane" -T fixture-task-1
second_agent_pane=$(
    "${tmux_test[@]}" new-window -d -t event-ui -n destination -P -F '#{pane_id}' \
        'sleep 300'
)
"${tmux_test[@]}" select-pane -t "$second_agent_pane" -T fixture-task-2
"${tmux_test[@]}" resize-window -t "$second_agent_pane" -x 100 -y 80
first_agent_pid=$(
    "${tmux_test[@]}" display-message -p -t "$first_agent_pane" '#{pane_pid}'
)
second_agent_pid=$(
    "${tmux_test[@]}" display-message -p -t "$second_agent_pane" '#{pane_pid}'
)
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

ui_pane=$(
    "${tmux_test[@]}" split-window -d -t "$first_agent_pane" -P -F '#{pane_id}' 'sleep 300'
)
printf -v ui_command 'env PATH=%q %q --config %q ui' \
    "$test_root/bin:$PATH" "$binary" "$config"
"${tmux_test[@]}" set-option -pt "$ui_pane" remain-on-exit on
"${tmux_test[@]}" respawn-pane -k -t "$ui_pane" "$ui_command"
second_ui_pane=$(
    "${tmux_test[@]}" split-window -d -t "$second_agent_pane" -P -F '#{pane_id}' 'sleep 300'
)
"${tmux_test[@]}" set-option -pt "$second_ui_pane" remain-on-exit on
"${tmux_test[@]}" respawn-pane -k -t "$second_ui_pane" "$ui_command"
"${tmux_test[@]}" select-window -t "$ui_pane"
for _ in {1..50}; do
    marker=$("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{@tmux_agent_ui}')
    second_marker=$(
        "${tmux_test[@]}" display-message -p -t "$second_ui_pane" '#{@tmux_agent_ui}'
    )
    [[ $marker == 1 && $second_marker == 1 ]] && break
    sleep 0.1
done
if [[ $marker != 1 || $second_marker != 1 ]]; then
    printf '%s\n' 'UI marker was not published' >&2
    exit 1
fi
if [[ $("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{pane_dead}') != 0 ]] ||
    [[ $("${tmux_test[@]}" display-message -p -t "$second_ui_pane" '#{pane_dead}') != 0 ]]; then
    printf '%s\n' 'UI exited before the topology change' >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$ui_pane" >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$second_ui_pane" >&2
    exit 1
fi
if "${tmux_test[@]}" capture-pane -p -t "$ui_pane" | grep -F 'CODEX' >/dev/null ||
    "${tmux_test[@]}" capture-pane -p -t "$second_ui_pane" | grep -F 'CODEX' >/dev/null; then
    printf '%s\n' 'synthetic Codex row must not exist before the topology change' >&2
    exit 1
fi
sleep 0.5

printf '%s\n' "$first_agent_pid" "$second_agent_pid" >"$fixture_pid"

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
    if [[ $("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{pane_dead}') == 1 ]] ||
        [[ $("${tmux_test[@]}" display-message -p -t "$second_ui_pane" '#{pane_dead}') == 1 ]]; then
        break
    fi
    if "${tmux_test[@]}" capture-pane -p -t "$ui_pane" | grep -F 'fixture-task-2' >/dev/null; then
        rendered=1
        break
    fi
    sleep 0.1
done

if [[ $("${tmux_test[@]}" display-message -p -t "$ui_pane" '#{pane_dead}') == 1 ]] ||
    [[ $("${tmux_test[@]}" display-message -p -t "$second_ui_pane" '#{pane_dead}') == 1 ]]; then
    printf '%s\n' 'event-driven UI exited after a topology-changing snapshot' >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$ui_pane" >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$second_ui_pane" >&2
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
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$second_ui_pane" >&2
    exit 1
fi

touch "$test_root/wake-block"
: >"$test_root/wake-started"
"${tmux_test[@]}" send-keys -t "$ui_pane" 2
focus_preceded_fanout=0
for _ in {1..100}; do
    wake_count=$(wc -l <"$test_root/wake-started" | tr -d ' ')
    destination_visible=$(
        "${tmux_test[@]}" display-message -p -t "$second_ui_pane" '#{window_active}'
    )
    if [[ $wake_count == 2 && $destination_visible == 1 ]]; then
        focus_preceded_fanout=1
        break
    fi
    sleep 0.01
done
touch "$test_root/wake-release"
if [[ $focus_preceded_fanout != 1 ]]; then
    printf '%s\n' 'numeric activation must focus before waking all sidebars concurrently' >&2
    printf 'wake_count=%s destination_visible=%s\n' "$wake_count" "$destination_visible" >&2
    sed -n '1,120p' "$test_root/tmux-calls" >&2
    exit 1
fi
selection_synced=0
for _ in {1..50}; do
    first_selection=$(
        "${tmux_test[@]}" capture-pane -p -t "$ui_pane" |
            grep '▌.*CODEX.*2 ' || true
    )
    second_selection=$(
        "${tmux_test[@]}" capture-pane -p -t "$second_ui_pane" |
            grep '▌.*CODEX.*2 ' || true
    )
    destination_visible=$(
        "${tmux_test[@]}" display-message -p -t "$second_ui_pane" '#{window_active}'
    )
    if [[ -n $first_selection && -n $second_selection && $destination_visible == 1 ]]; then
        selection_synced=1
        break
    fi
    sleep 0.1
done
if [[ $selection_synced != 1 ]]; then
    printf '%s\n' 'numeric selection was not synchronized across persistent UIs' >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$ui_pane" >&2
    "${tmux_test[@]}" capture-pane -p -S -80 -t "$second_ui_pane" >&2
    exit 1
fi

printf '%s\n' 'event-driven UI tests passed'
