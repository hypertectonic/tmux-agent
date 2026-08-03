#!/usr/bin/env bash

tmux_agent_root=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
tmux_agent_tmux=${TMUX_AGENT_TMUX_BIN:-tmux}

tmux_agent_option() {
    local name=$1
    local fallback=$2
    local setting
    local value
    setting=$("$tmux_agent_tmux" show-option -gq "$name" 2>/dev/null || true)
    if [[ -n $setting ]]; then
        value=$("$tmux_agent_tmux" show-option -gqv "$name" 2>/dev/null || true)
        printf '%s\n' "$value"
    else
        printf '%s\n' "$fallback"
    fi
}

tmux_agent_valid_dimension() {
    [[ $1 =~ ^([1-9][0-9]?|100)%$ || $1 =~ ^[1-9][0-9]*$ ]]
}

tmux_agent_binding() {
    local key=$1
    "$tmux_agent_tmux" list-keys -T prefix 2>/dev/null |
        awk -v key="$key" '$4 == key { print; exit }'
}

tmux_agent_key=$(tmux_agent_option @tmux-agent-key A)
tmux_agent_width=$(tmux_agent_option @tmux-agent-popup-width 80%)
tmux_agent_height=$(tmux_agent_option @tmux-agent-popup-height 80%)
tmux_agent_auto_start=$(tmux_agent_option @tmux-agent-auto-start on)
tmux_agent_binary=$("$tmux_agent_tmux" show-option -gqv @tmux-agent-binary 2>/dev/null || true)

if ! tmux_agent_valid_dimension "$tmux_agent_width"; then
    tmux_agent_width=80%
    "$tmux_agent_tmux" set-option -g @tmux-agent-popup-width "$tmux_agent_width"
    "$tmux_agent_tmux" display-message \
        'tmux-agent: invalid popup width; using 80%'
fi
if ! tmux_agent_valid_dimension "$tmux_agent_height"; then
    tmux_agent_height=80%
    "$tmux_agent_tmux" set-option -g @tmux-agent-popup-height "$tmux_agent_height"
    "$tmux_agent_tmux" display-message \
        'tmux-agent: invalid popup height; using 80%'
fi

tmux_agent_previous_key=$(
    "$tmux_agent_tmux" show-option -gqv @tmux-agent-bound-key 2>/dev/null || true
)
if [[ -n $tmux_agent_previous_key && $tmux_agent_previous_key != "$tmux_agent_key" ]]; then
    tmux_agent_previous_binding=$(tmux_agent_binding "$tmux_agent_previous_key")
    if [[ $tmux_agent_previous_binding == *TMUX_AGENT_PLUGIN_BINDING=1* ]]; then
        "$tmux_agent_tmux" unbind-key -T prefix "$tmux_agent_previous_key"
    fi
fi

if [[ -z $tmux_agent_key ]]; then
    "$tmux_agent_tmux" set-option -gu @tmux-agent-bound-key 2>/dev/null || true
else
    tmux_agent_existing_binding=$(tmux_agent_binding "$tmux_agent_key")
    if [[ -n $tmux_agent_existing_binding &&
        $tmux_agent_existing_binding != *TMUX_AGENT_PLUGIN_BINDING=1* ]]; then
        "$tmux_agent_tmux" display-message \
            "tmux-agent: prefix + $tmux_agent_key is already bound; set @tmux-agent-key to another key"
    else
        printf -v tmux_agent_launch_command '%q' \
            "$tmux_agent_root/scripts/launch-popup"
        tmux_agent_launch_command="TMUX_AGENT_PLUGIN_BINDING=1 $tmux_agent_launch_command"
        "$tmux_agent_tmux" bind-key -T prefix "$tmux_agent_key" \
            run-shell -b "$tmux_agent_launch_command"
        "$tmux_agent_tmux" set-option -g @tmux-agent-bound-key "$tmux_agent_key"
    fi
fi

tmux_agent_state_root=${TMUX_AGENT_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/tmux-agent}
mkdir -p "$tmux_agent_state_root"
chmod 700 "$tmux_agent_state_root"
tmux_agent_install_log="$tmux_agent_state_root/install.log"

if [[ -z $tmux_agent_binary ]]; then
    tmux_agent_binary="$tmux_agent_root/bin/tmux-agent"
    if [[ -n ${TMUX_AGENT_DATA_DIR:-} ]]; then
        tmux_agent_managed_binary="$TMUX_AGENT_DATA_DIR/current"
    elif [[ -n ${XDG_DATA_HOME:-} ]]; then
        tmux_agent_managed_binary="$XDG_DATA_HOME/tmux-agent/current"
    else
        tmux_agent_managed_binary="$HOME/.local/share/tmux-agent/current"
    fi
    tmux_agent_expected_version=$(sed -n '1p' "$tmux_agent_root/VERSION")
    tmux_agent_reported_version=$(
        "$tmux_agent_managed_binary" --version 2>/dev/null || true
    )
    if [[ $tmux_agent_reported_version != "tmux-agent $tmux_agent_expected_version" ]]; then
        "$tmux_agent_root/scripts/bootstrap" \
            >>"$tmux_agent_install_log" 2>&1 &
    fi
fi

case "$tmux_agent_auto_start" in
    on | true | 1 | yes)
        if [[ -x $tmux_agent_binary ]]; then
            if [[ -n $("$tmux_agent_tmux" show-option -gqv @tmux-agent-binary 2>/dev/null || true) ]]; then
                "$tmux_agent_binary" daemon start >>"$tmux_agent_install_log" 2>&1 &
            else
                "$tmux_agent_root/bin/tmux-agent" daemon start \
                    >>"$tmux_agent_install_log" 2>&1 &
            fi
        fi
        ;;
esac

unset tmux_agent_root tmux_agent_tmux tmux_agent_key tmux_agent_width
unset tmux_agent_height tmux_agent_auto_start tmux_agent_binary
unset tmux_agent_previous_key tmux_agent_previous_binding tmux_agent_existing_binding
unset tmux_agent_state_root tmux_agent_install_log tmux_agent_managed_binary
unset tmux_agent_launch_command
unset tmux_agent_expected_version tmux_agent_reported_version
unset -f tmux_agent_option tmux_agent_valid_dimension tmux_agent_binding
