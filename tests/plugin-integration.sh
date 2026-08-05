#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
command -v tmux >/dev/null 2>&1 || {
    printf '%s\n' 'tmux is required for plugin integration tests' >&2
    exit 1
}
cargo build --locked

test_root=$(mktemp -d "/tmp/tmux-agent-plugin-test.XXXXXX")
socket_name="tmux-agent-test-$$"
tmux_test=(tmux -L "$socket_name")

cleanup() {
    "${tmux_test[@]}" kill-server 2>/dev/null || true
    rm -rf -- "$test_root"
}
trap cleanup EXIT

mkdir -p "$test_root/runtime" "$test_root/state"
"${tmux_test[@]}" -f /dev/null new-session -d -s plugin-test
"${tmux_test[@]}" set-option -g @tmux-agent-auto-start off
"${tmux_test[@]}" set-option -g @tmux-agent-binary "$root/target/debug/tmux-agent"
"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"

binding=$("${tmux_test[@]}" list-keys -T prefix | awk '$4 == "A" { print; exit }')
[[ $binding == *scripts/launch-popup* ]]
"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"
[[ $("${tmux_test[@]}" list-keys -T prefix | awk '$4 == "A"' | wc -l | tr -d ' ') == 1 ]]

ordinary_pane=$("${tmux_test[@]}" display-message -p '#{pane_id}')
ui_pane=$("${tmux_test[@]}" split-window -d -P -F '#{pane_id}')
"${tmux_test[@]}" set-option -pt "$ui_pane" @tmux_agent_ui 1
"${tmux_test[@]}" select-pane -t "$ordinary_pane"
"${tmux_test[@]}" run-shell "$root/scripts/launch-popup"
[[ $("${tmux_test[@]}" display-message -p '#{pane_id}') == "$ui_pane" ]]

"${tmux_test[@]}" set-option -pt "$ordinary_pane" @tmux_agent_ui 1
"${tmux_test[@]}" select-pane -t "$ordinary_pane"
"${tmux_test[@]}" run-shell "$root/scripts/launch-popup" 2>/dev/null || true
[[ $("${tmux_test[@]}" display-message -p '#{pane_id}') == "$ordinary_pane" ]]
"${tmux_test[@]}" set-option -pu -t "$ordinary_pane" @tmux_agent_ui
"${tmux_test[@]}" select-pane -t "$ordinary_pane"
"${tmux_test[@]}" resize-pane -Z
"${tmux_test[@]}" run-shell "$root/scripts/launch-popup" 2>/dev/null || true
[[ $("${tmux_test[@]}" display-message -p '#{pane_id}') == "$ordinary_pane" ]]
"${tmux_test[@]}" resize-pane -Z

"${tmux_test[@]}" bind-key -T prefix B display-message user-binding
"${tmux_test[@]}" set-option -g @tmux-agent-key B
"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"
conflict=$("${tmux_test[@]}" list-keys -T prefix | awk '$4 == "B" { print; exit }')
[[ $conflict == *user-binding* ]]
[[ $conflict != *scripts/launch-popup* ]]

spaced_plugin="$test_root/plugin path"
mkdir -p "$spaced_plugin/scripts"
cp "$root/tmux-agent.tmux" "$spaced_plugin/tmux-agent.tmux"
cp "$root/scripts/launch-popup" "$spaced_plugin/scripts/launch-popup"
chmod +x "$spaced_plugin/tmux-agent.tmux" "$spaced_plugin/scripts/launch-popup"
"${tmux_test[@]}" unbind-key -T prefix C
"${tmux_test[@]}" set-option -g @tmux-agent-key C
printf -v spaced_entry_command '%q' "$spaced_plugin/tmux-agent.tmux"
"${tmux_test[@]}" run-shell "$spaced_entry_command"
spaced_binding=$(
    "${tmux_test[@]}" list-keys -T prefix | awk '$4 == "C" { print; exit }'
)
[[ $spaced_binding == *'plugin\\ path'* ]]

"${tmux_test[@]}" set-option -g @tmux-agent-popup-width invalid
"${tmux_test[@]}" set-option -g @tmux-agent-popup-height 0%
"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"
[[ $("${tmux_test[@]}" show-option -gqv @tmux-agent-popup-width) == 80% ]]
[[ $("${tmux_test[@]}" show-option -gqv @tmux-agent-popup-height) == 80% ]]

managed_data="$test_root/managed/tmux-agent"
managed_version=0.4.0
mkdir -p "$managed_data/versions/$managed_version"
cat >"$managed_data/versions/$managed_version/tmux-agent" <<EOF
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then
    printf '%s\\n' 'tmux-agent $managed_version'
fi
EOF
chmod +x "$managed_data/versions/$managed_version/tmux-agent"
cat >"$managed_data/versions/$managed_version/COMPATIBILITY" <<EOF
launcher_protocol=1
binary_version=$managed_version
EOF
ln -s "versions/$managed_version/tmux-agent" "$managed_data/current"
"${tmux_test[@]}" set-environment -g TMUX_AGENT_DATA_DIR "$managed_data"
"${tmux_test[@]}" set-option -gu @tmux-agent-binary
"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"
sleep 0.2
[[ $("$managed_data/current" --version) == "tmux-agent $managed_version" ]]
[[ ! -e $managed_data/install-status ]]
"${tmux_test[@]}" set-environment -gu TMUX_AGENT_DATA_DIR
"${tmux_test[@]}" set-option -g @tmux-agent-binary "$root/target/debug/tmux-agent"

"${tmux_test[@]}" set-environment -g XDG_RUNTIME_DIR "$test_root/runtime"
"${tmux_test[@]}" set-environment -g XDG_STATE_HOME "$test_root/state"
"${tmux_test[@]}" set-option -g @tmux-agent-auto-start on
"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"
socket_path=$("${tmux_test[@]}" display-message -p '#{socket_path}')
status=
for _ in {1..50}; do
    status=$(TMUX="$socket_path,0,0" \
        XDG_RUNTIME_DIR="$test_root/runtime" \
        XDG_STATE_HOME="$test_root/state" \
        "$root/target/debug/tmux-agent" daemon status)
    [[ $status == running:* ]] && break
    sleep 0.1
done
[[ $status == running:* ]]

"${tmux_test[@]}" run-shell "$root/tmux-agent.tmux"
TMUX="$socket_path,0,0" \
    XDG_RUNTIME_DIR="$test_root/runtime" \
    XDG_STATE_HOME="$test_root/state" \
    "$root/target/debug/tmux-agent" daemon stop >/dev/null

printf '%s\n' 'plugin integration tests passed'
