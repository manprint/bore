#!/usr/bin/env bash
# V-14: WHOSE ceiling is the relay's? -- attributing the relay arm's deficit
#
# THE QUESTION, AND WHY NO CAMPAIGN MAY SKIP IT
# ---------------------------------------------
# Wired, `vpn_ab.sh` measured the relay arm at 61.7 % of bare on download and
# 75.5 % on upload. Read naively that is a statement about bore's relay code.
# It is not, and the reason is structural rather than statistical:
#
#     bare    workstation <-> test VM                 ONE path
#     direct  workstation <-> test VM  (QUIC)         ONE path
#     relay   workstation <-> SERVER <-> test VM      TWO paths, THREE hosts
#
# The relay arm is not a third transport over the same path. It is a DOUBLE
# TRANSIT through a third machine, and that machine is a `t4g.micro` with 2
# vCPU whose cumulative allowance counters read, at the time of writing:
#
#     bw_in_allowance_exceeded:   11 808 249
#     bw_out_allowance_exceeded:       78 317
#     pps_allowance_exceeded:     26 595 715
#
# Those are cumulative since boot. They prove the instance exhausts its network
# allowance AT SOME POINT. They say nothing whatever about whether it did so
# during a given arm -- which is exactly the distinction between evidence and a
# story, and this repository already has a standing rule about it: CLAUDE.md
# records the same instance's allowance bucket as the surviving hypothesis for
# the vhost concurrency tail (N-9), reached only after every in-code mechanism
# had been falsified.
#
# Every campaign in this repository relays through this one server. So until
# this stage runs, NO relay percentage anywhere -- vhost, public, secret, VPN --
# is a sentence about bore.
#
# METHOD: FOUR DISCRIMINATORS, NOT ONE
# ------------------------------------
# A single counter can be argued with. These four cannot all be explained by the
# same alternative.
#
#  1. ALLOWANCE, AS A DELTA BRACKETING THE ARM. `asym_qualify.sh` established
#     the discipline for the VM and it applies here unchanged: read the three
#     counters immediately before and immediately after the relay arm, never
#     once at the end. A counter that is nonzero at the end cannot say which arm
#     spent it. A delta of zero across the arm rules the bucket out; a nonzero
#     delta convicts it.
#
#  2. THE TWO LEGS, MEASURED SEPARATELY. A relay cannot beat its slower leg.
#     `min(leg1, leg2)` is a hard ceiling that owes nothing to bore, and if the
#     relay figure sits at it there is no deficit left to attribute to code.
#     Leg 2 is driven FROM the server, so this workstation is not in that path
#     at all.
#
#  3. A NON-BORE DOUBLE TRANSIT (the control, and the strongest term). The same
#     two legs, chained on the same server by a relay containing no bore: no
#     yamux, no framing, no AEAD. It is the deployment shape with the product
#     removed. If it lands where bore lands, the ceiling is the deployment. If
#     bore is materially below it, THAT gap -- and only that gap -- is bore's,
#     and it is a finding worth acting on.
#
#  4. CPU ON THE RELAY HOST. Two vCPU forwarding both directions of a ~575
#     Mbit/s flow is not obviously comfortable. Sampled as a delta across the
#     arm, for the whole host and for the server process, so "the instance ran
#     out of CPU" is a measurement rather than an intuition.
#
# THE INSTRUMENT, AND WHY IT IS NOT iperf3 EVERYWHERE
# ---------------------------------------------------
# Discriminators 2 and 3 need a load endpoint and a TCP relay ON THE STAGING
# SERVER. That server has neither iperf3 nor socat, and installing packages on
# the host that carries the operator's live tunnels is not something a benchmark
# gets to decide. It does have python3 with `os.splice`, so `attrib_net.py`
# supplies both: the relay moves every byte socket -> pipe -> socket inside the
# kernel and Python never touches the data.
#
# A control relay that were itself the bottleneck would read as "the deployment
# is the ceiling" when the truth was "the control was slow" -- the most
# expensive way this stage could be wrong. So the instrument is CHECKED rather
# than trusted, twice over:
#
#   * `bare-py` runs the same instrument over the same workstation<->VM path
#     that `bare` measures with iperf3. The ratio between them is the
#     instrument's fidelity on a real path, measured rather than assumed.
#   * `leg1` is a SINGLE hop through the instrument, terminating on the very
#     server whose capacity is in question. If one hop reaches the link rate,
#     the instrument is not the limit at these speeds.
#
# If either check fails the control is reported as a FLOOR and no attribution is
# claimed from it. That verdict is printed by the stage, not left to the reader.
#
# WHAT THIS STAGE DELIBERATELY DOES NOT DO
# ----------------------------------------
# It does not restart, redeploy, reconfigure or install anything on the staging
# server. That server carries the operator's own live tunnels; a redeploy drops
# them and requires explicit approval. Everything here is read-only against the
# running server plus two short-lived listeners on unused high ports, torn down
# by RECORDED PID -- never by pattern (a pattern in an ssh command line matches
# the remote shell that is running it, which is how a `pkill -f` once killed the
# very launcher it was meant to protect).
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
SECS="${SECS:-10}"
PAR="${PAR:-4}"
PY_VM_PORT="${PY_VM_PORT:-5311}"      # duplex endpoint on the test VM
PY_SRV_PORT="${PY_SRV_PORT:-5312}"    # duplex endpoint on the staging server
PY_RELAY_PORT="${PY_RELAY_PORT:-5313}" # splice relay on the staging server -> VM
TOOL="$HERE/attrib_net.py"
RTOOL="attrib_net.py"                 # basename as deployed in the remote HOME

vpn_hdr "VPN relay attribution -- $REPS reps, ${SECS}s, P=$PAR"
echo "  bare/direct = workstation <-> VM.   relay = workstation <-> SERVER <-> VM."
echo "  the question is how much of the relay deficit belongs to the SERVER."
echo

[ -n "${BORE_SRV:-}" ] || { echo "BORE_SRV unset -- this stage is about that host"; exit 2; }
[ -f "$TOOL" ] || { echo "missing instrument $TOOL"; exit 2; }

# --- remote lifecycle -------------------------------------------------------
# Everything started remotely is recorded here and killed by PID at exit. Never
# by pattern: `ssh host "pkill -f X"` puts X in the remote shell's own argv.
SRV_PIDS=(); VM_PIDS=()

remote_cleanup() {
    local p
    for p in "${SRV_PIDS[@]:-}"; do
        [ -n "$p" ] && srv "kill -TERM $p 2>/dev/null; true" >/dev/null 2>&1
    done
    for p in "${VM_PIDS[@]:-}"; do
        [ -n "$p" ] && vm "kill -TERM $p 2>/dev/null; true" >/dev/null 2>&1
    done
}
# vpnlib installed its own EXIT trap; chain rather than replace it, or the VPN
# endpoints outlive the run and the host is left changed.
trap 'remote_cleanup; vpn_cleanup; vpn_assert_clean' EXIT
trap 'remote_cleanup; vpn_cleanup; vpn_assert_clean; exit 130' INT TERM

# All three allowance counters in ONE round trip, as a single line, so the
# before/after pair is two samples of the same quantity and not two shapes.
srv_allow() {
    srv "sudo -n ethtool -S $BORE_SRV_IFACE 2>/dev/null" 2>/dev/null \
      | awk '/bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded/ \
             {gsub(/:/,"",$1); printf "%s=%s ", $1, $2}'
}

# Host CPU as total/idle jiffies, plus the bore server process's own utime+stime.
# The process is found with a BRACKETED pattern: `ssh host "pgrep -f 'bore
# server'"` puts that string in the remote shell's own argv and matches itself.
srv_cpu() {
    srv "awk '/^cpu /{t=0; for(i=2;i<=NF;i++) t+=\$i; print \"tot\", t, \"idle\", \$5}' /proc/stat
         p=\$(ps -eo pid,args | awk '/[b]ore server/{print \$1; exit}')
         if [ -n \"\$p\" ]; then awk '{print \"proc\", \$14 + \$15}' /proc/\$p/stat 2>/dev/null; else echo 'proc na'; fi" \
      2>/dev/null | tr '\n' ' '
}

cpu_delta() { # before after secs -> one human line
    LC_ALL=C awk -v b="$1" -v a="$2" -v s="$3" 'BEGIN{
        nb=split(b,B," "); na=split(a,A," ")
        for(i=1;i<nb;i+=2) vb[B[i]]=B[i+1]
        for(i=1;i<na;i+=2) va[A[i]]=A[i+1]
        dt=va["tot"]-vb["tot"]; di=va["idle"]-vb["idle"]
        if (dt>0) printf "host %.1f%% busy", 100*(dt-di)/dt; else printf "host n/a"
        if (va["proc"] ~ /^[0-9]+$/ && vb["proc"] ~ /^[0-9]+$/ && s+0>0)
            printf ", bore %.2f core-s (%.0f%% of one core)", (va["proc"]-vb["proc"])/100, (va["proc"]-vb["proc"])/s
        else printf ", bore n/a (server pid not found)"
        printf "\n" }'
}

# --- provisioning -----------------------------------------------------------
echo "=== provisioning the control path (contains no bore) ==="

port_busy() { # host-fn port -> 0 when something is already listening
    case "$1" in
        srv) srv "ss -lnt 2>/dev/null | grep -q ':$2 '" >/dev/null 2>&1 ;;
        vm)  vm  "ss -lnt 2>/dev/null | grep -q ':$2 '" >/dev/null 2>&1 ;;
    esac
}

vmcp "$TOOL" "$BORE_VM_USER@$BORE_VM:~/$RTOOL"        >/dev/null 2>&1 || { echo "  scp to VM failed"; exit 1; }
vmcp "$TOOL" "$BORE_SRV_USER@$BORE_SRV:~/$RTOOL"      >/dev/null 2>&1 || { echo "  scp to server failed"; exit 1; }
echo "  instrument deployed to both remotes"

start_remote() { # srv|vm port "args..." -> echoes pid, or empty
    local where="$1" port="$2" args="$3" pid
    if port_busy "$where" "$port"; then
        echo "  $where: port $port already in use -- NOT touching it" >&2
        return 1
    fi
    case "$where" in
        srv) pid=$(srv "setsid nohup python3 ~/$RTOOL $args >/dev/null 2>&1 </dev/null & echo \$!" 2>/dev/null | tr -dc '0-9') ;;
        vm)  pid=$(vm  "setsid nohup python3 ~/$RTOOL $args >/dev/null 2>&1 </dev/null & echo \$!" 2>/dev/null | tr -dc '0-9') ;;
    esac
    sleep 1
    port_busy "$where" "$port" || { echo "  $where: $args FAILED to bind $port" >&2; return 1; }
    echo "$pid"
}

HAVE_LEGS=1; HAVE_CTRL=1
if pid=$(start_remote vm "$PY_VM_PORT" "duplex $PY_VM_PORT"); then
    VM_PIDS+=("$pid"); echo "  VM duplex endpoint on $PY_VM_PORT (pid $pid)"
else HAVE_LEGS=0; HAVE_CTRL=0; fi

if [ "$HAVE_LEGS" = 1 ]; then
    if pid=$(start_remote srv "$PY_SRV_PORT" "duplex $PY_SRV_PORT"); then
        SRV_PIDS+=("$pid"); echo "  server duplex endpoint on $PY_SRV_PORT (pid $pid)"
    else HAVE_LEGS=0; fi
fi

if [ "$HAVE_CTRL" = 1 ]; then
    if pid=$(start_remote srv "$PY_RELAY_PORT" "relay $PY_RELAY_PORT $BORE_VM $PY_VM_PORT"); then
        SRV_PIDS+=("$pid"); echo "  server splice relay $PY_RELAY_PORT -> VM:$PY_VM_PORT (pid $pid)"
    else HAVE_CTRL=0; fi
fi

# --- REACHABILITY, FROM THE SIDE THAT WILL MEASURE --------------------------
#
# `start_remote` verifies the BIND, on the host that serves. That is not the
# same question as whether the host that MEASURES can open a connection, and on
# this environment the two answers differ for every port this stage picks:
#
#   * 5311/5312/5313 are refused by the AWS security groups in EVERY direction
#     tested -- workstation -> VM, workstation -> server, and server -> VM. The
#     two AWS hosts cannot reach each other on an arbitrary port either.
#   * the public-tunnel range (9000+) IS admitted and still unreachable for a
#     helper, because the staging server runs bore in DOCKER and the nat
#     `DOCKER` chain DNATs that whole range into the container
#     (`tcp dport 9031 dnat to 172.18.0.2:9031`). A process listening on the
#     HOST receives nothing. That is precisely why `bore local --port 9031`
#     works: bore listens INSIDE the container.
#
# Without this gate the stage opened, for three repetitions, connections that
# could not succeed -- printing `0.00` for every python arm (kept out of the
# medians by `add()`, so no false number was published) and spending about two
# thirds of its wall clock on the attempts. A stage that cannot answer its
# question must say so in one line and skip, not retry.
#
# Unblocking it is an INFRASTRUCTURE change, not a script edit: open a port pair
# in the security groups for workstation<->VM, workstation<->server and
# server<->VM, and keep it outside the server's DNAT range.
probe_here() { # <host> <port>
    timeout 6 bash -c "exec 3<>/dev/tcp/$1/$2" 2>/dev/null
}
probe_from_srv() { # <host> <port>
    srv "timeout 6 bash -c 'exec 3<>/dev/tcp/$1/$2'" >/dev/null 2>&1
}

if [ "$HAVE_LEGS" = 1 ] || [ "$HAVE_CTRL" = 1 ]; then
    echo "  --- reachability (the bind above was checked on the SERVING host)"
    R_WS_VM=no;  probe_here "$BORE_VM"  "$PY_VM_PORT"    && R_WS_VM=yes
    R_WS_SRV=no; probe_here "$BORE_SRV" "$PY_SRV_PORT"   && R_WS_SRV=yes
    R_WS_REL=no; probe_here "$BORE_SRV" "$PY_RELAY_PORT" && R_WS_REL=yes
    R_SRV_VM=no; probe_from_srv "$BORE_VM" "$PY_VM_PORT" && R_SRV_VM=yes
    printf '      workstation -> VM:%s      %s\n'     "$PY_VM_PORT"    "$R_WS_VM"
    printf '      workstation -> server:%s  %s\n'     "$PY_SRV_PORT"   "$R_WS_SRV"
    printf '      workstation -> server:%s  %s\n'     "$PY_RELAY_PORT" "$R_WS_REL"
    printf '      server      -> VM:%s      %s\n'     "$PY_VM_PORT"    "$R_SRV_VM"
    # bare-py needs ws->VM; leg1 needs ws->server; leg2 needs server->VM.
    [ "$R_WS_VM" = yes ] && [ "$R_WS_SRV" = yes ] && [ "$R_SRV_VM" = yes ] || HAVE_LEGS=0
    # ctrl is ws -> server:relay -> VM, so it needs both hops.
    [ "$R_WS_REL" = yes ] && [ "$R_SRV_VM" = yes ] || HAVE_CTRL=0
fi

if [ "$HAVE_LEGS" = 0 ] && [ "$HAVE_CTRL" = 0 ]; then
    echo "  PER-LEG ATTRIBUTION UNAVAILABLE -- the instrument bound but nothing can"
    echo "  reach it. This is the security groups plus the server's Docker DNAT, not"
    echo "  a bug in the stage; see docs/performance/ETH_RERUN_EVIDENCE_2026-09-12.md"
    echo "  section 25.6. The bore relay arm below is still measured and still valid."
fi
[ "$HAVE_CTRL" = 1 ] || echo "  CONTROL RELAY UNAVAILABLE -- the strongest discriminator is missing."
echo

pyc()  { python3 "$TOOL" client "$1" "$2" "$3" "$SECS" "$PAR" 2>/dev/null; }   # from the workstation
pysrv() { srv "python3 ~/$RTOOL client $1 $2 $3 $SECS $PAR" 2>/dev/null; }     # from the server

# --- the bore relay arm -----------------------------------------------------
bring_up_relay() {
    VPN_LINK_ID="${VPN_RUN_ID}$(date +%s%N | tail -c 6)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()
    vm_up listen --relay-only
    sleep 3
    ws_up at connect --relay-only
    ws_ready at 45 >/dev/null || { echo "    link did not come up"; return 1; }
    ARM_MTU="$(wait_mtu_settle at 24 90)"
    return 0
}

declare -A R
add() { case "$2" in ''|0|0.00|FAILED) return;; esac; R["$1"]+=" $2"; }

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"

    # 0. bare, with the campaign's own instrument, in this repetition.
    if [ "$(vm_iperf_server)" = 1 ]; then
        d=$(tcp_mbps "$BORE_VM" "$SECS" "$PAR" -R); sleep 2
        u=$(tcp_mbps "$BORE_VM" "$SECS" "$PAR");    sleep 2
        printf '    %-13s down %-9s up %-9s  (iperf3)\n' bare "$d" "$u"
        add "bare|down" "$d"; add "bare|up" "$u"
    else
        echo "    bare          FAILED (no iperf3 server on the VM)"
    fi

    # 0b. the SAME path with the python instrument: its fidelity, measured.
    if [ "$HAVE_LEGS" = 1 ] || [ "$HAVE_CTRL" = 1 ]; then
        d=$(pyc "$BORE_VM" "$PY_VM_PORT" down); sleep 2
        u=$(pyc "$BORE_VM" "$PY_VM_PORT" up);   sleep 2
        printf '    %-13s down %-9s up %-9s  (python, same path as bare)\n' bare-py "${d:-FAILED}" "${u:-FAILED}"
        add "barepy|down" "$d"; add "barepy|up" "$u"
    fi

    # 1. leg 1: workstation <-> staging server. A SINGLE hop terminating on the
    #    host in question -- so it is both a leg and the instrument check.
    if [ "$HAVE_LEGS" = 1 ]; then
        b=$(srv_allow); cb=$(srv_cpu)
        d=$(pyc "$BORE_SRV" "$PY_SRV_PORT" down); sleep 2
        u=$(pyc "$BORE_SRV" "$PY_SRV_PORT" up)
        a=$(srv_allow); ca=$(srv_cpu)
        printf '    %-13s down %-9s up %-9s  (one hop, ws<->srv)\n' "leg1" "${d:-FAILED}" "${u:-FAILED}"
        printf '    %-13s allowance before[%s] after[%s]\n' "" "${b:-n/a}" "${a:-n/a}"
        printf '    %-13s cpu %s' "" "$(cpu_delta "$cb" "$ca" $((SECS * 2 + 2)))"
        add "leg1|down" "$d"; add "leg1|up" "$u"
        sleep 2
    fi

    # 2. leg 2: staging server <-> test VM, driven FROM the server, so this
    #    workstation's access link is not in the path at all.
    if [ "$HAVE_LEGS" = 1 ]; then
        d=$(pysrv "$BORE_VM" "$PY_VM_PORT" down); sleep 2
        u=$(pysrv "$BORE_VM" "$PY_VM_PORT" up);   sleep 2
        printf '    %-13s down %-9s up %-9s  (workstation not in this path)\n' "leg2" "${d:-FAILED}" "${u:-FAILED}"
        add "leg2|down" "$d"; add "leg2|up" "$u"
    fi

    # 3. the control: the same two legs, chained on the same server, no bore.
    if [ "$HAVE_CTRL" = 1 ]; then
        b=$(srv_allow); cb=$(srv_cpu)
        d=$(pyc "$BORE_SRV" "$PY_RELAY_PORT" down); sleep 2
        u=$(pyc "$BORE_SRV" "$PY_RELAY_PORT" up)
        a=$(srv_allow); ca=$(srv_cpu)
        printf '    %-13s down %-9s up %-9s  (splice relay, ws->srv->vm)\n' "ctrl" "${d:-FAILED}" "${u:-FAILED}"
        printf '    %-13s allowance before[%s] after[%s]\n' "" "${b:-n/a}" "${a:-n/a}"
        printf '    %-13s cpu %s' "" "$(cpu_delta "$cb" "$ca" $((SECS * 2 + 2)))"
        add "ctrl|down" "$d"; add "ctrl|up" "$u"
        sleep 2
    fi

    # 4. bore's own relay arm, bracketed exactly the same way.
    if bring_up_relay; then
        if [ "$(vm_iperf_server)" = 1 ]; then
            b=$(srv_allow); cb=$(srv_cpu)
            d=$(tcp_mbps "$B_PEER" "$SECS" "$PAR" -R); sleep 2
            u=$(tcp_mbps "$B_PEER" "$SECS" "$PAR")
            a=$(srv_allow); ca=$(srv_cpu)
            path="$(ws_path at)"
            if [ "$path" != relay ]; then
                printf '    %-13s FAILED -- path is %s, not relay\n' "bore-relay" "$path"
            else
                printf '    %-13s down %-9s up %-9s  mtu %s (iperf3)\n' "bore-relay" "$d" "$u" "${ARM_MTU:-?}"
                printf '    %-13s allowance before[%s] after[%s]\n' "" "${b:-n/a}" "${a:-n/a}"
                printf '    %-13s cpu %s' "" "$(cpu_delta "$cb" "$ca" $((SECS * 2 + 2)))"
                add "relay|down" "$d"; add "relay|up" "$u"
            fi
        else
            echo "    bore-relay    FAILED (no iperf3 server on the VM)"
        fi
    fi
    vpn_cleanup; sleep 3
done

echo
echo "=== medians (Mbit/s) ==="
printf '  %-12s %-12s %-12s  %s\n' arm download upload what
m() { printf '%s\n' ${R["$1"]:-} | med; }
printf '  %-12s %-12s %-12s  %s\n' bare       "$(m bare\|down)"   "$(m bare\|up)"   "ws<->vm, iperf3 (the campaign reference)"
printf '  %-12s %-12s %-12s  %s\n' bare-py    "$(m barepy\|down)" "$(m barepy\|up)" "ws<->vm, python (instrument fidelity)"
printf '  %-12s %-12s %-12s  %s\n' leg1       "$(m leg1\|down)"   "$(m leg1\|up)"   "ws<->srv, one hop"
printf '  %-12s %-12s %-12s  %s\n' leg2       "$(m leg2\|down)"   "$(m leg2\|up)"   "srv<->vm, ws not in path"
printf '  %-12s %-12s %-12s  %s\n' ctrl       "$(m ctrl\|down)"   "$(m ctrl\|up)"   "ws->srv->vm, no bore"
printf '  %-12s %-12s %-12s  %s\n' bore-relay "$(m relay\|down)"  "$(m relay\|up)"  "ws->srv->vm, bore VPN relay"

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for k in "${!R[@]}"; do printf '  %-14s%s\n' "$k" "${R[$k]}"; done | sort

echo
echo "=== reading ==="
LC_ALL=C awk \
  -v bd="$(m bare\|down)"   -v bu="$(m bare\|up)" \
  -v pd="$(m barepy\|down)" -v pu="$(m barepy\|up)" \
  -v l1d="$(m leg1\|down)"  -v l1u="$(m leg1\|up)" \
  -v l2d="$(m leg2\|down)"  -v l2u="$(m leg2\|up)" \
  -v cd="$(m ctrl\|down)"   -v cu="$(m ctrl\|up)" \
  -v rd="$(m relay\|down)"  -v ru="$(m relay\|up)" 'BEGIN{
  num="^[0-9.]+$"
  ok=1

  # --- the instrument, before anything is concluded from it
  if (pd ~ num && bd ~ num && bd>0) {
      f = pd/bd
      printf "  instrument: python reads %.0f%% of iperf3 on the identical path.\n", 100*f
      if (f < 0.9) { ok=0
          print "              BELOW 90% -- the python figures are a FLOOR, not a capacity." }
  } else { ok=0; print "  instrument: NOT calibrated this run (bare-py or bare missing)." }
  if (l1d ~ num && bd ~ num && bd>0 && l1d < 0.8*bd) {
      printf "  instrument: one hop to the server reads %.0f%% of bare -- either the server\n", 100*l1d/bd
      print  "              leg is genuinely narrower, or the instrument is. Not separable here."
  }

  # --- the hard ceiling that owes nothing to bore
  if (l1d ~ num && l2d ~ num) {
      md=(l1d<l2d?l1d:l2d); mu=((l1u ~ num && l2u ~ num) ? (l1u<l2u?l1u:l2u) : -1)
      printf "  slower leg: download %.1f", md
      if (mu>0) printf ", upload %.1f", mu
      printf " Mbit/s -- a relay cannot beat this.\n"
      if (rd ~ num && md>0) printf "  bore relay reaches %.0f%% of the slower leg on download.\n", 100*rd/md
  }

  # --- bore against the campaign reference
  if (bd ~ num && rd ~ num && bd>0) printf "  bore relay is %.0f%% of bare on download", 100*rd/bd
  if (bu ~ num && ru ~ num && bu>0) printf ", %.0f%% on upload", 100*ru/bu
  if (bd ~ num && rd ~ num && bd>0) printf ".\n"

  # --- the control, which is the whole point
  if (!(cd ~ num) || !(rd ~ num) || cd+0<=0) {
      print "  THE CONTROL DID NOT PRODUCE A NUMBER. Without a non-bore double transit"
      print "  through the same server, the split between deployment and code is NOT"
      print "  measured here and must not be asserted."
      exit
  }
  r = rd/cd
  printf "  bore relay / splice relay (identical path, no bore): %.3f download", r
  if (cu ~ num && ru ~ num && cu+0>0) printf ", %.3f upload", ru/cu
  printf "\n"
  if (!ok) {
      print "  => the instrument did not calibrate, so this ratio is DIRECTIONAL ONLY."
      exit
  }
  if (r > 0.92) {
      print "  => at parity with a plain kernel-splice relay on the same host."
      print "     The ceiling is the DEPLOYMENT -- a double transit through a 2-vCPU"
      print "     instance -- and not the bore relay code."
  } else {
      printf "  => bore is %.0f%% below a plain kernel-splice relay over the identical\n", 100*(1-r)
      print  "     path. That gap, and only that gap, belongs to bore."
  }
}'
echo
echo "  Allowance: a delta of ZERO across an arm rules the instance's token bucket"
echo "  out for THAT ARM. A nonzero delta convicts it. The cumulative totals mean"
echo "  nothing here -- they cannot say which arm spent them."
echo
echo "DONE"
