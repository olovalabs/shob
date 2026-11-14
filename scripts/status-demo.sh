#!/usr/bin/env bash
# Simulate each terminal status and watch the sidebar/titlebar badge.
#
#   scripts/status-demo.sh active    focused prompt            -> blue  (idle once you click away)
#   scripts/status-demo.sh working   long command with spinner -> blue while focused, yellow after you switch away
#   scripts/status-demo.sh blocked   approval prompt           -> orange
#   scripts/status-demo.sh error     failing command output    -> red
#   scripts/status-demo.sh agent     agent-shaped run, exits 0 -> green (done) if you are in another tab
#
# Run one case per terminal tab. `exit 0` at the shell prompt is the other way
# to get done: it ends the terminal's own process.
set -u

case "${1:-help}" in
active)
    echo "type here: blue while this tab is focused, gray after you click another tab"
    read -r -p "press enter to finish " _
    ;;

working)
    # A plain foreground command: Active while you watch it, Working when you
    # switch away. An agent rule that reports Working stays yellow either way.
    for i in $(seq 1 30); do
        printf '⠋ Working... (%ds · esc to interrupt)\r' "$i"
        sleep 1
    done
    echo ""
    ;;

blocked)
    # Shape of live_blocked_form: a decision waiting on the user.
    read -r -p "Do you want to proceed? [y/n] " _
    echo "answered"
    ;;

error)
    # Strong failure markers on recent lines -> red, sticky until you look.
    # Stay in this tab and the badge clears on the next tick (you saw it);
    # switch tabs within a second to see the red badge stay.
    echo "error: simulated failure" >&2
    sh -c 'exit 1'
    ;;

agent | done)
    # A fake agent CLI. Detection keys on the process name, so run a `sleep`
    # under a `claude` symlink: switch to another tab before it exits and the
    # badge turns green, then back to this tab and it clears to active.
    tmp="$(mktemp -d)"
    ln -sf "$(command -v sleep)" "$tmp/claude"
    "$tmp/claude" 6
    rm -rf "$tmp"
    echo "agent finished"
    ;;

*)
    sed -n '2,13p' "$0"
    ;;
esac
