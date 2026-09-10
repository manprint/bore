#!/usr/bin/env bash
# columns: ts user nice sys idle iowait irq softirq steal load
awk 'NF>=10{
  t=$1; u=$2+$3; s=$4; id=$5; io=$6; ir=$7+$8; st=$9; ld=$10
  tot=u+s+id+io+ir+st
  if(pt){ d=tot-pt; if(d>0) printf "t+%-4d busy=%5.1f%%  usr=%5.1f  sys=%5.1f  softirq=%5.1f  STEAL=%5.1f%%  load=%s\n", t-t0, 100*((u-pu)+(s-ps)+(ir-pir)+(st-pst))/d, 100*(u-pu)/d, 100*(s-ps)/d, 100*(ir-pir)/d, 100*(st-pst)/d, ld }
  else t0=t
  pt=tot; pu=u; ps=s; pir=ir; pst=st
}' "$1"
