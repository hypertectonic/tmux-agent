#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
command -v tmux >/dev/null 2>&1 || {
    printf '%s\n' 'tmux is required for remote pane binding tests' >&2
    exit 1
}
real_tmux=$(command -v tmux)
cargo build --locked

test_root=$(mktemp -d "/tmp/tmux-agent-remote-binding-test.XXXXXX")
socket_name="tmux-agent-remote-binding-$$"
tmux_tmp="$test_root/tmux"
tmux_test=(env TMUX_TMPDIR="$tmux_tmp" "$real_tmux" -L "$socket_name")
binary="$root/target/debug/tmux-agent"
config="$test_root/config.toml"

cleanup() {
    exit_code=$?
    "${tmux_test[@]}" kill-server >/dev/null 2>&1 || true
    rm -rf -- "$test_root"
    return "$exit_code"
}
trap cleanup EXIT

mkdir -p "$test_root/home" "$test_root/runtime" "$test_root/state" "$tmux_tmp"
cat >"$config" <<EOF
tmux_args = ["-L", "$socket_name"]

[[remote]]
name = "thinkcat"
command = ["ssh", "thinkcat", "tmux-agent", "watch", "--jsonl", "--local-only"]
EOF

run_isolated() {
    env \
        HOME="$test_root/home" \
        TMUX_TMPDIR="$tmux_tmp" \
        XDG_RUNTIME_DIR="$test_root/runtime" \
        XDG_STATE_HOME="$test_root/state" \
        "$binary" --config "$config" "$@"
}

transport_pane=$(
    "${tmux_test[@]}" new-session -d -x 100 -y 30 -s local \
        -P -F '#{pane_id}' 'sleep 300'
)
ui_pane=$("${tmux_test[@]}" split-window -d -t "$transport_pane" -P -F '#{pane_id}' 'sleep 300')
"${tmux_test[@]}" set-option -pt "$ui_pane" @tmux_agent_ui 1

if run_isolated remote bind unknown remote-session --pane "$transport_pane" \
    >"$test_root/unknown.stdout" 2>"$test_root/unknown.stderr"; then
    printf '%s\n' 'unknown remote alias must be rejected' >&2
    exit 1
fi
grep -F 'no configured remote named "unknown"' "$test_root/unknown.stderr" >/dev/null

bound=$(run_isolated remote bind thinkcat tmux-agent-res --pane "$transport_pane")
[[ $bound == "bound $transport_pane to thinkcat/tmux-agent-res" ]]
[[ $("${tmux_test[@]}" show-option -pqv -t "$transport_pane" @tmux_agent_remote_host) == thinkcat ]]
[[ $("${tmux_test[@]}" show-option -pqv -t "$transport_pane" @tmux_agent_remote_session) == tmux-agent-res ]]
[[ $(run_isolated remote bindings) == "$transport_pane thinkcat tmux-agent-res" ]]

if run_isolated remote bind thinkcat other-session --pane "$ui_pane" \
    >"$test_root/ui.stdout" 2>"$test_root/ui.stderr"; then
    printf '%s\n' 'tmux-agent UI panes must not become remote bindings' >&2
    exit 1
fi
grep -F "$ui_pane is a tmux-agent UI pane" "$test_root/ui.stderr" >/dev/null

unbound=$(run_isolated remote unbind --pane "$transport_pane")
[[ $unbound == "unbound $transport_pane" ]]
[[ -z $("${tmux_test[@]}" show-option -pqv -t "$transport_pane" @tmux_agent_remote_host) ]]
[[ -z $("${tmux_test[@]}" show-option -pqv -t "$transport_pane" @tmux_agent_remote_session) ]]
[[ $(run_isolated remote bindings) == 'No remote pane bindings.' ]]

bound=$(TMUX_PANE="$transport_pane" run_isolated remote bind thinkcat default-session)
[[ $bound == "bound $transport_pane to thinkcat/default-session" ]]

printf '%s\n' 'remote pane binding tests passed'
