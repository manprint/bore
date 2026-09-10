#!/usr/bin/env bash
# Client-side path impairment for the staging vhost benchmark.
#
# Adds delay/loss on EGRESS traffic toward ONE destination (the staging server),
# leaving every other flow on this workstation untouched. The impaired leg is
# provider -> server, which is the leg that actually carries the response body of
# a tunnelled download, so this is enough to compare the TCP relay against the
# QUIC direct path under loss without setting up ingress shaping.
#
# Usage:
#   sudo -n /abs/path/scripts/vhost_staging_netem.sh apply <server-ip> <delay-ms> <loss-%> [iface] [proto]
#
# proto selects which transport is impaired: all (default), tcp, or udp.
# Impairing one protocol at a time separates the two legs that a tunnelled
# transfer uses. The tunnel data plane is TCP for the relay and UDP for the
# QUIC direct path, while the browser leg is always TCP, so "udp" impairs only
# the direct data plane and "tcp" impairs the relay plus the browser leg.
#   sudo -n /abs/path/scripts/vhost_staging_netem.sh clear [iface]
#   sudo -n /abs/path/scripts/vhost_staging_netem.sh show  [iface]
#
# The qdisc is removed by `clear`; nothing is persisted across reboots.
set -euo pipefail

TC=/usr/sbin/tc
[ -x "$TC" ] || TC=$(command -v tc)

default_iface() { /usr/sbin/ip route get 1.1.1.1 | awk '{print $5; exit}'; }

cmd="${1:-show}"
case "$cmd" in
  apply)
    ip="${2:?server ip}"; delay="${3:?delay ms}"; loss="${4:?loss percent}"
    IF="${5:-$(default_iface)}"; PROTO="${6:-all}"
    case "$PROTO" in
      all) pmatch="" ;;
      tcp) pmatch="match ip protocol 6 0xff" ;;
      udp) pmatch="match ip protocol 17 0xff" ;;
      *) echo "proto must be all, tcp or udp" >&2; exit 1 ;;
    esac
    $TC qdisc del dev "$IF" root 2>/dev/null || true
    $TC qdisc add dev "$IF" root handle 1: prio bands 3
    # band 3 carries only the impaired destination; bands 1-2 stay unshaped
    if [ "$loss" = "0" ]; then
        $TC qdisc add dev "$IF" parent 1:3 handle 30: netem delay "${delay}ms"
    else
        $TC qdisc add dev "$IF" parent 1:3 handle 30: netem delay "${delay}ms" loss "${loss}%"
    fi
    # shellcheck disable=SC2086 -- pmatch is an intentional multi-word fragment
    $TC filter add dev "$IF" protocol ip parent 1:0 prio 1 u32 \
        match ip dst "$ip/32" $pmatch flowid 1:3
    echo "impaired egress to $ip on $IF ($PROTO): +${delay}ms, ${loss}% loss"
    ;;
  clear)
    IF="${2:-$(default_iface)}"
    $TC qdisc del dev "$IF" root 2>/dev/null || true
    echo "cleared qdisc on $IF"
    ;;
  show)
    IF="${2:-$(default_iface)}"
    $TC -s qdisc show dev "$IF" | head -20
    $TC filter show dev "$IF" 2>/dev/null | head -10
    ;;
  *) echo "usage: $0 apply <ip> <delay-ms> <loss-%> [iface] [all|tcp|udp] | clear [iface] | show [iface]" >&2; exit 1 ;;
esac
