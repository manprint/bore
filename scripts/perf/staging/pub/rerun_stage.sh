#!/usr/bin/env bash
# Re-run ONE or MORE campaign stages after the campaign has already finished,
# with the sampling and the reduction that the stage needs.
#
# Why this exists. `run_campaign.sh` stops the resource samplers when it
# finishes, which is correct for a campaign but wrong for a re-run: the CPU
# stage prints `window=<t0>-<t1>` lines whose only meaning comes from a host
# /proc/stat sample stream covering the same window, and with the samplers
# stopped those windows reduce to "no samples in the window" — a whole stage
# re-run for nothing (harness defect H-10, found exactly that way).
#
# So this script does the three things a re-run needs, in order:
#   1. pushes the current harness to the VM and proves the copies match by
#      md5 (a re-run against a stale script is H-7 all over again);
#   2. makes sure the samplers are alive BEFORE the stage starts, and says so;
#   3. runs the stages serially on the VM, collects the logs and the samples,
#      and — for the `eff` stage — joins the windows to the CPU samples itself,
#      so the CPU s/GiB table is produced rather than assembled by hand.
#
#   rerun_stage.sh eff
#   rerun_stage.sh conc eff flavours_udp
#   DEST=out/pub-20260911-055454 rerun_stage.sh eff      # collect into an existing run
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../lib.sh"

[ "$#" -gt 0 ] || { echo "usage: rerun_stage.sh <stage> [stage...]"; exit 2; }
STAGES="$*"
DEST="${DEST:-$OUT/rerun-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$DEST"

say "harness -> VM (and prove the copies match)"
vmcp "$HERE"/*.sh "$BORE_VM_USER@$BORE_VM:~/pub/" >/dev/null 2>&1
vmcp "$HERE/../../raw_origin.py" "$HERE/../../raw_client.py" \
     "$BORE_VM_USER@$BORE_VM:~/" >/dev/null 2>&1
vm 'chmod +x ~/pub/*.sh ~/raw_origin.py ~/raw_client.py' >/dev/null 2>&1
bad=0
for f in raw_origin.py raw_client.py; do
    a=$(md5sum "$HERE/../../$f" | cut -d' ' -f1)
    b=$(vm "md5sum ~/$f" 2>/dev/null | tr -d '\r' | cut -d' ' -f1)
    [ "$a" = "$b" ] || { echo "  MISMATCH $f local=$a vm=$b"; bad=1; }
done
for f in "$HERE"/*.sh; do
    n=$(basename "$f")
    a=$(md5sum "$f" | cut -d' ' -f1)
    b=$(vm "md5sum ~/pub/$n" 2>/dev/null | tr -d '\r' | cut -d' ' -f1)
    [ "$a" = "$b" ] || { echo "  MISMATCH $n local=$a vm=$b"; bad=1; }
done
[ "$bad" = 0 ] && echo "  every copy matches" || { echo "  refusing to run against a stale harness"; exit 1; }

say "samplers"
alive=$(srv 'pgrep -c -f "res_sample[r].sh" 2>/dev/null || echo 0' 2>/dev/null | tr -d '\r')
last=$(srv 'tail -1 ~/pres.stat 2>/dev/null | cut -d" " -f1' 2>/dev/null | tr -d '\r')
now=$(date +%s)
if [ "${alive:-0}" -lt 1 ] || [ $(( now - ${last:-0} )) -gt 30 ]; then
    echo "  the server sampler is not running (or is stale): starting fresh"
    "$HERE/../res/start_samplers.sh" 20000 2>&1 | sed 's/^/    /'
else
    echo "  already sampling (server's last sample $(( now - last ))s ago)"
fi

say "build under test"
{
  adm config | jq -r '"version_field=\(.server_version // "not reported")"'
  srv "sudo -n docker exec ${BORE_SRV_CONTAINER} /bore --version 2>/dev/null" 2>/dev/null
  # shellcheck disable=SC2088  # the tilde is expanded by the REMOTE shell, which is the point
  echo "vm_binary=$(vm '~/bore --version' 2>/dev/null | tr -d '\r')"
  echo "stages=$STAGES"
  echo "started=$(date -Is)"
} | tee "$DEST/BUILD.txt"

say "VM stages: $STAGES"
# `pub_driver.log` is APPENDED to by the driver and is what the wait loop
# below tails, so it is truncated first: left in place, its last line is the
# previous campaign's "ALL PUBLIC STAGES DONE" and the progress echo would
# report a finished run for the whole re-run.
vm "rm -f ~/res/rerun_driver.log ~/res/pub_driver.log; setsid nohup ~/pub/pub_driver.sh $STAGES > ~/res/rerun_driver.log 2>&1 </dev/null & true" >/dev/null 2>&1
# The pattern is BRACKETED (`pub_drive[r]`) because the remote command string
# itself contains the name being searched for: an unbracketed `pgrep -f
# pub_driver.sh` matches the very `bash -c` that is running the pgrep, so the
# count never falls to zero and the wait never ends. `start_samplers.sh`
# carries a comment about the same self-match trap in its pkill; this loop had
# to learn it too.
#
# H-13: and then it had to learn a SECOND one, which cost four hours of nothing.
# `pgrep -c` prints the count — including `0` — and ALSO exits 1 when nothing
# matched, so a `|| echo 0` fallback fires on top of the zero it already
# printed and `n` comes back as the two lines "0\n0". That is not a comparison
# that fails, it is a SYNTAX error in `test`:
#
#     $ [ "0
#     0" -lt 1 ]; echo $?
#     bash: [: 0\n0: integer expression expected
#     2
#
# rc=2 is falsy, so `&& break` never fires and the loop spins forever on a
# stage that finished minutes ago — silently, because everything it prints is
# the same progress line. MEASURED: a `conc` re-run sat in this loop for four
# hours after its stage had returned rc=0, and was found only by listing
# processes. THREE things fix it, and all three are needed:
#   1. drop the `|| echo 0` and take the FIRST line, so the count is one token;
#   2. refuse a non-numeric count LOUDLY instead of treating it as zero — an
#      ssh that fails mid-run must not look like "the stage finished";
#   3. bound the whole wait with a deadline, because no amount of parsing care
#      covers a VM that dies with its driver still registered.
DEADLINE=$(( $(date +%s) + ${RERUN_MAX_WAIT:-10800} ))
while true; do
    sleep 30
    n=$(vm 'pgrep -c -f "pub_drive[r].sh" 2>/dev/null; true' 2>/dev/null | tr -d '\r' | head -1)
    echo "  $(date -Is) $(vm 'tail -1 ~/res/pub_driver.log 2>/dev/null' 2>/dev/null | tr -d '\r')"
    case "$n" in
        ''|*[!0-9]*)
            echo "  WARNING: cannot read the driver count from the VM (got '$n') — still waiting" ;;
        *)
            [ "$n" -lt 1 ] && break ;;
    esac
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        echo "  GIVING UP: the VM driver is still registered after ${RERUN_MAX_WAIT:-10800}s."
        echo "  Collecting whatever the stage produced; check ~/res/pub_driver.log on the VM."
        break
    fi
done

say "collecting"
for s in $STAGES; do
    vmcp "$BORE_VM_USER@$BORE_VM:~/res/pub_${s}.log" "$DEST/" >/dev/null 2>&1
done
vmcp "$BORE_VM_USER@$BORE_VM:~/res/pub_driver.log" "$DEST/" >/dev/null 2>&1
srv 'cat ~/pres.stat' > "$DEST/pres_srv.stat" 2>/dev/null
srv 'cat ~/pres.proc' > "$DEST/pres_srv.proc" 2>/dev/null
vm  'cat ~/pres.stat' > "$DEST/pres_vm.stat"  2>/dev/null
echo "  $(wc -l < "$DEST/pres_srv.stat") server samples, $(wc -l < "$DEST/pres_vm.stat") VM samples"

# The join that used to be done by hand. Every `CASE` line carries its own
# window and its own GiB, so the reduction is mechanical — and a stage whose
# samples are missing says so here instead of in a document.
if [ -s "$DEST/pub_eff.log" ]; then
    say "P9 CPU cost per GiB (server host, softirq included, steal excluded)"
    printf '  %-11s %-7s %9s %9s %9s %9s %9s\n' case path GiB busy_s cpu_s_GiB cores steal_s
    while read -r tag path win _dur _by _rate _unit gib; do
        t0=${win#window=}; t1=${t0#*-}; t0=${t0%-*}
        out=$("$HERE/../res/cpu_window.sh" "$DEST/pres_srv.stat" "$t0" "$t1" "${gib#gib=}" 2>&1)
        case "$out" in
            *cpu_s_per_gib=*)
                LC_ALL=C awk -v t="${tag}" -v p="${path#path=}" -v g="${gib#gib=}" '{
                    for (i=1;i<=NF;i++) { split($i,kv,"="); v[kv[1]]=kv[2] }
                    printf "  %-11s %-7s %9.3f %9.2f %9.2f %9.2f %9.2f\n",
                           t, p, g, v["busy"], v["cpu_s_per_gib"], v["cores_busy"], v["steal"]
                }' <<<"$out" ;;
            *) echo "  $tag $path: $out" ;;
        esac
    done < <(grep '^CASE' "$DEST/pub_eff.log" | sed 's/^CASE //')
fi

echo "finished=$(date -Is)" >> "$DEST/BUILD.txt"
say "artefacts in $DEST"
ls -la "$DEST"
