#!/usr/bin/env bash
# The contention guard, ONCE.
#
# Two campaign drivers must never share the link (they contend for the single
# line being measured) and the netns harnesses must never overlap (they share
# the ns0/ns1/ns2 names, so one's pre-cleanup wipes the other's namespaces
# mid-run and fabricates failures). Every driver therefore refuses to start
# beside another.
#
# WHY THIS IS A FILE AND NOT A FUNCTION COPIED FOUR TIMES
# -------------------------------------------------------
# It WAS copied four times, and the copies shared a matching rule that is
# structurally wrong: `ps -eo pid,args | awk '$0 ~ /rerun_eth\.sh/'` matches any
# command line that merely CONTAINS the name, anywhere, in any field.
#
# MEASURED 2026-09-13: with P7 finished and nothing else running, re-invoking
# `rerun_eth_p7.sh` printed `REFUSING: the main sweep is still running` -- the
# process it found was a MONITOR whose `bash -c` body mentioned the driver's
# name while watching its log. A guard that stops the campaign because
# something is READING ABOUT it is not a guard, and this is the third time in
# this campaign that matching a process by text has produced a wrong answer
# (a `pkill` that killed the session issuing it; a guard that matched its own
# command-substitution subshell).
#
# THE RULE: a process is RUNNING a driver only if the driver's filename is one
# of the first two arguments of its argv -- `./x/rerun_eth.sh`, or `bash
# ./x/rerun_eth.sh`, or `nohup ./x/rerun_eth.sh`. A `bash -c '<text>'` puts the
# text in argv[2] onward and can never match, no matter what the text says.
# Read from /proc/<pid>/cmdline, which is NUL-separated and therefore
# unambiguous about where one argument ends -- unlike `ps args`, where a
# filename and a sentence about it look identical.
#
# It never kills anything: it reports what it found and the caller stands down.

DRIVER_NAMES="${DRIVER_NAMES:-rerun_eth.sh rerun_eth_p7.sh rerun_vpn_deep.sh rerun_jump.sh rerun_open.sh p5_build_gate.sh}"

# Print "<pid> <argv>" for every OTHER process that is running a campaign
# driver. Prints nothing when the link is free.
other_driver() {
    # Self and ancestors are excluded by pid. `$$` alone is NOT enough:
    # `found="$(other_driver)"` runs this function in a COMMAND-SUBSTITUTION
    # SUBSHELL, which is a fork of bash carrying the same argv -- so it appears
    # under this very script's name with a pid that is neither `$$` (bash keeps
    # the original shell's pid there) nor an ancestor of it. MEASURED: with `$$`
    # alone the check named its own subshell as the offender on every run.
    local excl=" $$ $BASHPID " p=$PPID pid f1 f2 args
    while [ -n "$p" ] && [ "$p" != 0 ] && [ "$p" != 1 ]; do
        excl+="$p "
        p=$(awk '{print $4}' "/proc/$p/stat" 2>/dev/null)
    done

    for d in /proc/[0-9]*; do
        pid="${d#/proc/}"
        case "$excl" in *" $pid "*) continue ;; esac
        [ -r "$d/cmdline" ] || continue
        # argv[0] and argv[1], basenamed. Nothing else is a launch position.
        f1="$(tr '\0' '\n' < "$d/cmdline" 2>/dev/null | sed -n '1p')"
        f2="$(tr '\0' '\n' < "$d/cmdline" 2>/dev/null | sed -n '2p')"
        for n in $DRIVER_NAMES; do
            if [ "${f1##*/}" = "$n" ] || [ "${f2##*/}" = "$n" ]; then
                args="$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)"
                printf '%s %s\n' "$pid" "$args"
                break
            fi
        done
    done
}
