#!/usr/bin/env bash
# Sequential driver for the VM-side stages. Strictly serial: every stage shares
# one server and one 2-vCPU VM, so overlapping them would contaminate both.
set -uo pipefail
H=$VM_HOME; R=$H/res; mkdir -p "$R"
run(){ local name=$1; shift
  echo "### $(date -Is) START $name" >> "$R/driver.log"
  "$@" > "$R/$name.log" 2>&1
  echo "### $(date -Is) END   $name rc=$?" >> "$R/driver.log"
  sleep 10
}
run s3_ab        $H/vm_ab.sh
run s4_stab      $H/vm_stab.sh all
run s5_netem     $H/vm_netem.sh
run s6_ssh       $H/vm_ssh.sh
run s7_eff       $H/vm_eff.sh
echo "ALL STAGES DONE $(date -Is)" >> "$R/driver.log"
