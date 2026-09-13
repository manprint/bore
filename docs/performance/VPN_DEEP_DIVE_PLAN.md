# VPN — piano di copertura completa (deep dive)

**Obiettivo dichiarato:** la VPN non deve avere difetti. Quindi la domanda non è
"va veloce", è **"quale parte del prodotto non è mai stata misurata"**.

Questo documento non è una lista di desideri: la matrice qui sotto è stata
**estratta dal sorgente** (`VpnListenArgs` / `VpnConnectArgs` in `src/main.rs`,
più le variabili `BORE_*` lette da `src/vpn.rs`, `src/holepunch.rs`,
`src/hostcfg.rs`) e incrociata con le fasi che passano davvero quell'opzione.
Rigenerabile:

```sh
cd scripts/perf/staging/vpn
for o in advertise relay-only carriers tun-queues max-clients ...; do
    printf '%-28s %s\n' "--$o" "$(grep -l -- "--$o" *.sh | tr '\n' ' ')"
done
```

---

## 1. Matrice delle opzioni CLI

| opzione | fasi che la esercitano | giudizio |
|---|---|---|
| `--relay-only` | `vpn_ab`, `vpn_direct_deficit`, `vpn_lat`, `vpn_profile`, `vpn_rtt_load`, `vpn_relay_attrib` | **coperta** |
| `--mtu` / `--pin-mtu` | `vpnlib` (tutte) | **coperta** |
| `--vpn-addr` / `--vpn-peer-addr` | `vpnlib` (tutte) | **coperta** |
| `--tun-queues` | `vpn_wire_ceiling`, `vpnlib` | **coperta** |
| `--auto-reconnect` | `vpn_stability`, `vpn_ctrl_leak` | **coperta** |
| `--max-clients` (hub) | `vpn_hub` | **parziale** — vedi §3 |
| `--advertise` (gateway + netmap) | `vpn_modes`, `vpn_hub` | **parziale** — vedi §3 |
| `--nat-masquerade` | `vpn_modes` | **parziale** (stesso motivo) |
| `--forward-accept` | `vpn_modes` | **parziale** (stesso motivo) |
| `--accept-all-routes` | `vpn_modes`, `vpn_hub` | coperta |
| `--carriers` | `vpn_wire_ceiling` | **debole** — un solo contesto, e V-13 lo ha già falsificato sul diretto |
| `--accept-route` / `--refuse-route` | — | **SCOPERTA** |
| `--no-route-manage` | — | **SCOPERTA** |
| `--tun-name` | — | **SCOPERTA** |
| `--stun-server` | — | **SCOPERTA** |
| `--upnp` | — | **SCOPERTA** |
| `--try-port-prediction` | — | **SCOPERTA** |
| `--nat-udp-preferred-port` | — | **SCOPERTA** |
| `--nat-udp-release-timeout` | — | **SCOPERTA** |
| `--insecure` | — | scoperta, ma è TLS del controllo: non è una domanda di prestazioni |
| `--notes` | — | scoperta, puramente informativa |

## 2. Matrice delle variabili d'ambiente

| knob | fasi | giudizio |
|---|---|---|
| `BORE_DIRECT_DGRAM_SEND_BUF` | `vpn_sndbuf`, `vpn_udpbuf`, `vpn_cc_matrix` | **coperta** (scala piatta da cablato) |
| `BORE_DIRECT_UDP_SEND_BUF` | `vpn_udpbuf`, `vpn_cc_matrix` | **coperta** (idem) |
| `BORE_DIRECT_QUIC_CC` | `vpn_cc_matrix`, `vpn_wire_ceiling` | coperta, ma la matrice va rifatta (§9 evidenze) |
| `BORE_DIRECT_QUIC_GSO` | `vpn_cc_matrix` | coperta |
| **`BORE_VPN_TUN_TXQUEUELEN`** | — | **SCOPERTA, ed è un buco serio: vedi §4** |
| `BORE_DIRECT_QUIC_IDLE_MS` | — | SCOPERTA |
| `BORE_DIRECT_QUIC_KEEPALIVE_MS` | — | SCOPERTA |
| `BORE_DIRECT_QUIC_INITIAL_RTT_MS` | — | SCOPERTA (S-7) |
| `BORE_DIRECT_QUIC_ACK_THRESHOLD` | solo lato secret (`sec_ack`) | scoperta **sulla VPN** |
| `BORE_UDP_SPRAY_*` | — | SCOPERTA (Fase 7) |
| `BORE_VPN_FORCE_IPTABLES` | — | scoperta; è un selettore di backend, correttezza non banda |
| `BORE_PROXY_BUFFER_SIZE` | — | scoperta; non sul percorso dati VPN |

---

## 3. Il buco che attraversa tre righe "parziale": nessuna misura oltre il gateway

`vpn_modes.sh` stampa, ad ogni esecuzione:

```
  NOTE: LAN_HOST unset -- the FORWARD-chain probe is skipped.
        The lanaddr/netmap arms below price PREROUTING + the rewrite, NOT forwarding.
```

Quindi `--advertise`, `--nat-masquerade` e `--forward-accept` sono stati misurati
**solo fino al gateway stesso**. Il caso d'uso reale — un host di LAN *dietro* il
gateway — non è mai stato sul percorso dati. E nella stessa esecuzione la fase ha
riportato `far end FORWARD policy: DROP`, che è esattamente la condizione per cui
`--forward-accept` esiste: su quella coppia un host reale dietro il gateway
sarebbe stato **irraggiungibile**.

Stessa forma per l'hub: i due spoke girano entrambi su questa workstation, quindi
l'isolamento fra spoke non è misurabile lì (il kernel risponde all'indirizzo
locale senza passare dall'hub — §14 delle evidenze).

**Cosa serve:** un terzo host. Le opzioni, in ordine di costo:

1. una **seconda VM** nella stessa regione come "host di LAN" dietro la VM
   gateway — risolve sia `LAN_HOST` sia il secondo spoke dell'hub;
2. un **container/netns sulla VM** con la VM come router — più economico, copre
   il forwarding ma non l'isolamento fra host distinti;
3. un host della LAN di casa dietro la workstation — copre la direzione opposta.

Senza uno di questi, tre opzioni di prodotto restano dichiarate e non misurate.

---

## 4. `BORE_VPN_TUN_TXQUEUELEN`: la scala misura il kernel, non il prodotto

`vpn_txqueue.sh` cambia il valore così:

```sh
sudo -n ip link set dev "$IF" txqueuelen "$q"
```

cioè **scavalcando bore**. La scala misurata è quindi valida come fisica della
coda, ma non dimostra nessuna delle due cose che contano per il prodotto:

* che `create_tun` applichi davvero `VPN_TUN_TXQUEUELEN = 128` al link che crea;
* che `BORE_VPN_TUN_TXQUEUELEN` (con i suoi limiti: `0` = lascia il kernel,
  altrimenti clamp `[16, 500]`) arrivi fino al device.

Esiste il gate unitario `tun_txqueuelen_resolution`, che copre la **risoluzione**
del valore, non il **cablaggio**. È esattamente la distinzione che questo
progetto applica altrove (P-12: "il log prova che il client ne ha parlato, la API
prova che il server ci crede").

**Misura che manca, ed è banale:** portare su un link con i default e leggere
`ip -o link show dev boreN` — il kernel, mai il log — attendendosi 128; poi
rifarlo con `BORE_VPN_TUN_TXQUEUELEN=64`, con `=0` (deve restare il default del
kernel, 500) e con un valore fuori scala (deve essere clampato, non rifiutato).
Quattro letture, nessun traffico.

---

## 5. Ordine di esecuzione proposto

Prima le cose che non richiedono infrastruttura nuova:

| # | fase | risponde a | costo | stato |
|---|---|---|---|---|
| D1 | `vpn_wiring.sh` — cablaggio dei knob letto dal kernel | §4, più MTU e code | minuti, niente traffico | **scritta** |
| D2 | `vpn_traversal_opts.sh` — `--stun-server`, `--nat-udp-preferred-port`, `--try-port-prediction`, `--upnp` | 4 opzioni scoperte; effetto su **time-to-direct**, non su banda | medio | **scritta** |
| D3 | `vpn_routes.sh` — `--accept-routes` / `--refuse-routes` / `--no-route-manage` | policy default-deny I-MC8: una rotta rifiutata **non deve** comparire | basso, niente traffico | **scritta** |
| D4 | `vpn_quic_timers.sh` — idle / keepalive / initial-rtt | 3 knob scoperti; effetto su ripresa e su TTD | medio | **scritta** |
| D5 | `vpn_carriers.sh` — carrier su **relay** e su direct, 1/2/4 × 1/4 flussi | `--carriers` ha un solo contesto oggi | medio | **scritta** |

Tutte e cinque girano dal driver `scripts/perf/staging/rerun_vpn_deep.sh`, che
si rifiuta di partire se un altro driver della campagna è vivo.

### Due cose che D2 e D4 hanno richiesto, e vale la pena registrare

**D2 non misura un flag: misura che il flag abbia agito.** Ogni braccio porta,
accanto alla sua opzione, la *riga di log che ne è la prova* (`port prediction
ENABLED`, `managed port mapping ENABLED`, l'indirizzo locale sulla porta
preferita, il server STUN selezionato). Un braccio senza quella riga è stampato
`NOT-APPLIED` e tenuto fuori da ogni mediana. Senza questa regola `--upnp` su un
router senza PCP né IGD avrebbe prodotto un risultato nullo perfettamente
credibile su una funzione mai esercitata — la stessa forma dell'errore di
`vpn_hub` (§14 delle evidenze).

**D4 ha richiesto un nuovo verbo nell'unico punto d'ingresso root.**
`scripts/vpn_tun_endpoint.sh blackhole on|off|status <ipv4>` installa una tabella
nft dedicata che scarta l'UDP da e verso il solo estremo remoto: il percorso
diretto muore, il relay TCP verso il *server* — un altro host — resta vivo. È
l'unico stimolo che, sul percorso reale, esercita la promessa DEC-2 (ritorno al
relay caldo **in place**, senza riconnessione) e l'unico modo di dare un prezzo a
`BORE_DIRECT_QUIC_IDLE_MS` / `_KEEPALIVE_MS`. La regola vive in una tabella con
un nome proprio (rimozione = una `delete table`), `vpn_cleanup` la toglie su ogni
uscita e `vpn_assert_clean` dichiara sporca la macchina se sopravvive — una
blackhole dimenticata è invisibile a `ip route` e rovinerebbe in silenzio ogni
misura successiva.

Poi quelle che richiedono il terzo host (§3):

| # | fase | richiede |
|---|---|---|
| D6 | `vpn_modes` con `LAN_HOST` impostato | seconda VM o netns sulla VM |
| D7 | `vpn_hub` con spoke su host distinti | seconda VM |

**Regola che vale per tutte:** ogni fase confronta contro un controllo nudo
campionato **nella stessa ripetizione** (V-9), attende la stabilizzazione della
MTU prima di misurare, stampa i campioni grezzi accanto alla mediana (V-11), e
dichiara FAILED un braccio che non ha raggiunto il percorso che il suo nome
dichiara.
