#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/tmux-agent-popup-test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT
mkdir -p "$test_root/path with spaces"
log="$test_root/tmux.log"
fake_tmux="$test_root/path with spaces/tmux"
fake_binary="$test_root/path with spaces/tmux agent"

cat >"$fake_binary" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$fake_binary"

cat >"$fake_tmux" <<'EOF'
#!/bin/sh
case "$1" in
    show-option)
        case "$3" in
            @tmux-agent-popup-width) printf '%s\n' "${TEST_WIDTH:-72%}" ;;
            @tmux-agent-popup-height) printf '%s\n' "${TEST_HEIGHT:-61%}" ;;
            @tmux-agent-binary) printf '%s\n' "$TEST_BINARY" ;;
        esac
        ;;
    list-panes)
        case "${TEST_PANES:-none}" in
            one) printf '%s\n' '%7' ;;
            multiple) printf '%s\n' '%7' '%8' ;;
        esac
        ;;
    select-pane|display-popup|display-message)
        printf '%s' "$1" >>"$TEST_LOG"
        shift
        for argument in "$@"; do
            printf '\t%s' "$argument" >>"$TEST_LOG"
        done
        printf '\n' >>"$TEST_LOG"
        ;;
    *)
        printf 'unexpected fake tmux command: %s\n' "$*" >&2
        exit 1
        ;;
esac
EOF
chmod +x "$fake_tmux"

export TEST_BINARY="$fake_binary"
export TEST_LOG="$log"
export TMUX_AGENT_TMUX_BIN="$fake_tmux"
launcher=(/bin/bash "$root/scripts/launch-popup")

TEST_PANES=one "${launcher[@]}"
grep -F $'select-pane\t-t\t%7' "$log" >/dev/null

: >"$log"
TEST_PANES=none "${launcher[@]}"
popup=$(grep '^display-popup' "$log")
[[ $popup == *$'\t-w\t72%\t-h\t61%'* ]]
[[ $popup == *"tmux\\ agent ui --popup"* ]]

: >"$log"
if TEST_PANES=multiple "${launcher[@]}"; then
    printf '%s\n' 'multiple UI panes should be rejected' >&2
    exit 1
fi
grep -F 'multiple UI panes are visible' "$log" >/dev/null

: >"$log"
TEST_PANES=none TEST_WIDTH=bad TEST_HEIGHT=0% "${launcher[@]}"
popup=$(grep '^display-popup' "$log")
[[ $popup == *$'\t-w\t80%\t-h\t80%'* ]]

printf '%s\n' 'popup launcher tests passed'
