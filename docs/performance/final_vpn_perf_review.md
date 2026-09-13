# VPN — revisione di performance, traversal e affidabilità, 2026-09-12

> Documento finale della campagna sulla **VPN** di bore (`bore vpn listen` /
> `bore vpn connect`, feature `vpn`), l'unico sottosistema che porta pacchetti
> IP grezzi dentro un tunnel e quindi l'unico in cui il collo di bottiglia non
> è "quanti byte al secondo" ma **quanta coda c'è in mezzo**.
>
> Le prove grezze stanno in
> [`VPN_EVIDENCE_2026-09-12.md`](VPN_EVIDENCE_2026-09-12.md); la meccanica del
> traversal sta in [`../nat/NAT_TRAVERSAL.md`](../nat/NAT_TRAVERSAL.md) e il
> comportamento del prodotto in [`../vpn/VPN.md`](../vpn/VPN.md). Qui si tirano
> le somme: cosa è stato misurato, cosa era rotto, cosa è stato cambiato, cosa
> resta aperto e perché.

---

## 0. Come rifare tutto fra un mese

Nessuno script di questa campagna contiene un host, una porta o una
credenziale: tutto arriva da `env.sh` (`$BORE_PERF_ENV`, poi
`~/.config/bore-perf/env.sh`, poi `./env.sh`; `chmod 600`, **mai** nel
repository — il modello è `scripts/perf/staging/env.sh.example`).

```shell
# 0. PRIMA di qualsiasi numero assoluto: qualificare la linea di accesso
scripts/perf/staging/vpn/link_baseline.sh        | tee out/link-baseline-$(date +%F).txt

# 1. i cancelli in netns (UNO ALLA VOLTA, mai due insieme: ns0/ns1/ns2 condivisi)
cargo build --release --features vpn             # il cancello rifiuta un binario vecchio
sudo -n /abs/path/scripts/vpn_netns_test.sh
sudo -n /abs/path/scripts/udp_nat_netns_test.sh

# 2. la campagna sul percorso reale (workstation ↔ VM AWS, stessa regione)
scripts/perf/staging/vpn/vpn_ab.sh               | tee out/vpn/ab.txt
scripts/perf/staging/vpn/vpn_profile.sh          | tee out/vpn/profile.txt
scripts/perf/staging/vpn/vpn_lat.sh              | tee out/vpn/lat.txt
scripts/perf/staging/vpn/vpn_txqueue.sh          | tee out/vpn/txqueue.txt
scripts/perf/staging/vpn/vpn_cc_matrix.sh        | tee out/vpn/cc-matrix.txt
scripts/perf/staging/vpn/vpn_wire_ceiling.sh     | tee out/vpn/wire-ceiling.txt
scripts/perf/staging/vpn/vpn_direct_deficit.sh   | tee out/vpn/direct-deficit.txt
scripts/perf/staging/vpn/vpn_hub.sh              | tee out/vpn/hub.txt
scripts/perf/staging/vpn/vpn_modes.sh            | tee out/vpn/modes.txt
scripts/perf/staging/vpn/vpn_stability.sh        | tee out/vpn/stability.txt
```

**`link_baseline.sh` va per primo in qualunque giornata di cui si citino numeri
assoluti.** Non è burocrazia: §11b delle prove è il racconto di una serie di
misure che descrivevano il WiFi di casa e non il tunnel.

Root serve solo per creare un TUN, quindi è l'unica parte che gira da root:
`scripts/vpn_tun_endpoint.sh` avvia/ferma **un** endpoint locale e nient'altro.
I driver girano non privilegiati. Su questa workstation conta, perché `sudo` è
concesso **per percorso esatto** e con `env_reset`.

---

## 1. Il risultato in una riga

Il percorso diretto della VPN è passato da **62,7 % a 70,6 %** della banda
nuda in upload, e per la prima volta batte il relay in entrambe le direzioni.
La causa del deficit non era la crittografia, né il controllo di congestione,
né i buffer che due campagne precedenti avevano già setacciato: era **la coda
di trasmissione del dispositivo TUN**, che il kernel crea con un default
sbagliato per questo carico. Resta aperto un **secondo** deficit, di natura
diversa, documentato al §6.

---

## 2. Cosa c'era di rotto (e non di lento)

Tre difetti veri, tutti trovati misurando, nessuno visibile da una lettura del
codice.

### 2.1 — Hub mode era indietro di un'intera generazione di traversal (V-1)

`--max-clients N>1` offriva al broker una lista di candidati **nuda**. Senza
profilo NAT strutturato il server non poteva calcolare il piano adattivo,
senza piano nessuno dei due lati poteva costruire una `CheckConfig`, e quindi
ogni spoke cadeva sul **punch cieco** legacy: niente round autenticato, niente
ordinamento per gruppi, niente fuga sprayed (Fase 7), niente cache della coppia
vincente, niente S-5. Era perfettamente auto-consistente, ed è esattamente per
questo che non falliva mai e nessuno se ne era accorto.

La correzione conserva l'offerta intera (`VpnProviderEntry.hub_offer`) invece
di ridurla alla lista di indirizzi, e fa arrivare allo spoke il rider che il
broker ha calcolato. Il percorso 1:1 non è toccato.

### 2.2 — Il diagnostico attraversava diversamente dal prodotto (V-2)

`bore test-udp --tcp-secret-id` è lo strumento che si usa **quando un tunnel
vero non va in diretto**. Non eseguiva il round autenticato: andava dritto al
punch cieco. Su una cella che il round vince e il punch cieco non può vincere
(`eim:adf × edm`, misurata sul kernel) il diagnostico rispondeva **relay** per
una coppia il cui tunnel va **diretto**. È l'errore più costoso che questo
strumento possa fare, perché è l'unico oracolo che l'operatore ha.

Ora `establish_direct` entra nelle **stesse** funzioni del prodotto
(`listener_checks_then_quic` / `dialer_checks_then_quic`). Tre decisioni sono
politica, non meccanica, e ognuna ha il suo cancello:

* il round è **condizionato alla capability del PEER** (`UdpTestPeerSummary.checks`,
  campo additivo `#[serde(default)]`): un round che l'altro capo non sa
  rispondere è indistinguibile da una rete che si è mangiata i frame, cioè
  proprio il falso negativo che questo strumento esiste per non produrre;
* la **cache della coppia vincente non viene consultata** di proposito: ogni
  esecuzione misura una coppia fredda, che è quello che serve a chi diagnostica;
* il ruolo dello spray si prende **solo** da un piano calcolato dal server,
  perché due piani calcolati localmente non sono garantiti complementari e due
  lati "easy" si spruzzano addosso senza aprire nessun filtro.

### 2.3 — Il deficit di upload del percorso diretto era la coda del TUN (V-10)

Un TUN ha **due** code in serie: la qdisc e la coda skb del dispositivo,
limitata da `txqueuelen`, che è quella da cui legge l'applicazione. La qdisc
accoda **solo quando la coda del dispositivo è piena**, quindi una coda di
dispositivo profonda rende `fq_codel` inerte: `tc -s qdisc show` riporta
`backlog 0b 0p` mentre il percorso porta centinaia di millisecondi. È per
questo che due ladder di buffer precedenti l'avevano mancata.

Il limite conta **pacchetti**, e con l'offload TUN attivo un elemento è un
super-pacchetto GSO (**misurato ~34 KB**: 603 MB in 17 827 elementi). Il
default del kernel, 500, vale fino a ~17 MB, cioè ~360 ms a 375 Mbit/s.

Misurato sul percorso reale, un solo link tenuto per tutta la scala:

| `txqueuelen` | upload | rtt sotto carico |
|---:|---:|---:|
| 500 (default kernel) | 248,3 Mbit/s | 147 ms |
| 256 | 272,3 | 111 |
| 192 | 277,6 | 114 |
| **128 (nuovo default)** | 265,7 | 97 |
| 64 | 258,2 | 92 |
| 32 | 91,5 | — |
| 8 | 5,6 | — |

**Il default del kernel è il gradino peggiore su entrambi gli assi.** Il nuovo
default è 128 e non 64 perché sotto c'è un dirupo: il valore sta in mezzo alla
regione utile, non sul suo bordo. Override: `BORE_VPN_TUN_TXQUEUELEN`
(`0` = non toccare il valore del kernel; altrimenti limitato a `[16, 500]`).

---

## 3. Cosa è stato falsificato (e perciò non cambiato)

Vale quanto le correzioni, perché ognuna di queste era una modifica plausibile
che qualcuno avrebbe fatto "per buon senso".

| ipotesi | misura | esito |
|---|---|---|
| il buffer di invio dei datagram QUIC (8 MiB) causa bufferbloat | ladder 8 / 2 / 0,5 MiB con **TCP interno**, throughput e rtt nella stessa trasmissione | 16× di profondità non muovono né l'uno né l'altro (V-3) — ma vedi §6: sotto un'offerta UDP fissa il buffer **conta**, e le due misure sono entrambe giuste |
| il controllo di congestione va cambiato | 4 ripetizioni interlacciate, `newreno` / `cubic` / `bbr` | `newreno` +3,3 % ma alza il **minimo** di rtt sotto carico (26,1 → 33,4 ms): resta `bbr` (V-12) |
| un endpoint è CPU-bound | campionamento per processo e per thread su entrambi i capi | 13–19 % di **un** core a 400 Mbit/s (V-8, V-13) |
| più carrier aiutano un flusso | ladder di flussi, `--carriers` 1/2/4 | un singolo flusso vive su un carrier per costruzione (BW-F2): nessun guadagno (V-7) |
| il percorso diretto aggiunge latenza | rtt a vuoto, diretto vs relay | il diretto **toglie** latenza (V-5) |

Sul controllo di congestione la decisione è registrata come tale in
`CLAUDE.md`: il premio è +3,3 % contro un deficit del 30 %, un controller
basato sulle perdite non è mai stato misurato su un percorso con perdite, e
`BORE_DIRECT_QUIC_CC` resta la via d'uscita per chi ha qualificato la propria
linea. **Non si riapre su un numero di throughput a percorso singolo; si
riapre con un percorso lossy nella matrice.**

---

## 4. Le regole di misura che hanno salvato la campagna

Quattro sono ereditate dalle campagne precedenti. Due sono nate qui, e sono
nate da errori veri.

1. **Qualificare la linea prima di citare un valore assoluto** (V-9). I
   rapporti contro un controllo nudo campionato nella **stessa** ripetizione
   valgono su qualunque linea; i Mbit/s no. `link_baseline.sh` legge Ookla (con
   un server dentro l'ISP stesso), due CDN, iperf3 nudo a P=1 **e** P=8, i
   contatori di errore della radio come **delta** su traffico vero, e i
   contatori di allowance ENA dell'altro capo. *P=1 contro P=8 è la diagnosi in
   una riga*: un limite per flusso si apre con il parallelismo, un policer no.
2. **`sort -n` dipende dalla locale e corrompe le mediane in silenzio** (V-11).
   Sotto `it_IT.UTF-8`, `{397.46, 264.01, 408}` si ordina `408, 264.01, 397.46`
   e la mediana legge 264,01. `LC_ALL=C` è ora esportato da `vpnlib.sh` così che
   un helper definito dentro un singolo stadio non possa reintrodurlo. **Regola
   conseguente: un harness che stampa una statistica deve stampare anche i
   campioni** — questo bug è invisibile in un file di sole mediane, ed è
   esattamente così che è stato preso.
3. **`iperf3 -u` riporta la banda OFFERTA, non quella consegnata.** A 540
   Mbit/s offerti con il 22 % di perdita stampa comunque 539,9. Consegnato è
   `offerto × (1 − perdita)`, e lo stadio stampa entrambe le colonne.
4. **In un harness `set -euo pipefail`, il `grep` di un'asserzione non deve
   poter terminare la corsa.** "Nessuna corrispondenza" è uscita 1, che è un
   esito normalissimo per un controllo che ha il diritto di fallire — ma sotto
   `set -e` uccide lo script, e poiché il trap EXIT esegue comunque la pulizia
   lo script **esce 0**. La suite si ferma lì e sembra passata. Misurato sul
   nuovo cancello `T-NAT-DIAG-ROUND`: 26 righe PASS, nessun FAIL, exit 0 — e le
   tre celle Fase 7 e il totale finale saltati. **Corollario: una corsa verde
   che non finisce con il proprio totale PASS/FAIL non è stata letta.**

---

## 5. Cancelli

Tutto quello che segue è stato eseguito su questa workstation, in serie (mai
due harness netns insieme: condividono i nomi `ns0`/`ns1`/`ns2`, e la
pre-pulizia di uno cancella i namespace dell'altro a metà corsa fabbricando
fallimenti).

| cancello | esito |
|---|---|
| `cargo fmt --all` | pulito |
| `cargo clippy --all-targets -- -D warnings` (default, `--features vpn`, `--all-features`) | verde nelle tre configurazioni |
| `cargo test --all-features` | **1025 passati, 0 falliti** |
| `scripts/udp_nat_netns_test.sh` | **PASS 29 / FAIL 0** |
| `scripts/vpn_netns_test.sh` | **PASS 161 / FAIL 0** |

I cancelli nuovi di questa campagna, e cosa provano che gli altri non provano:

* `tun_txqueuelen_resolution` — unità: fissa la **politica** del nuovo default
  e dei limiti dell'override.
* `check_config_is_none_for_a_peer_that_cannot_answer_the_round`,
  `spray_role_is_taken_only_from_a_server_brokered_plan`,
  `typed_peer_candidates_refuses_a_length_mismatch` — unità: fissano le tre
  regole di politica di V-2. **Tutti e tre red-checked**: annullando ciascuna
  delle tre regole fallisce esattamente il suo test e nessun altro.
* `T-NAT-DIAG-ROUND` (netns) — fissa il **cablaggio**: due `bore test-udp`
  appaiati attraverso due NAT veri, e si verifica sia che il round sia girato
  su **entrambi** i lati (la capability si legge dal sommario del peer, quindi
  un passaggio a metà è proprio lo stato che questo cancello rifiuta) sia che
  il verdetto del diagnostico **coincida** con quello del prodotto sulla stessa
  cella.

Lo standard è quello del repository: **l'unità fissa la politica, il netns
fissa il cablaggio.** Un test in-process non può distinguere le due cose qui,
perché il difetto è una frase su due peer dietro due NAT reali.

---

## 6. Il secondo deficit (V-13): misurato, e la domanda si è dissolta

Dopo V-10 restava un buco: il tunnel portava ~375 Mbit/s dove l'UDP nudo sulla
stessa quintupla ne portava 540. Ogni misura precedente era stata presa con un
**flusso TCP interno**, che è lo strumento sbagliato per un tetto: il TCP
interno *reagisce* alla quantità che si vuole misurare, quindi un tubo con un
limite duro e un tubo semplicemente controllato in congestione producono lo
stesso numero. Lo stadio `vpn_wire_ceiling.sh` guida il tunnel con UDP a
**banda offerta fissa**: un tubo con un tetto consegna il tetto e perde il
resto; un tubo con margine consegna quello che gli viene offerto.

### Le due ipotesi strutturali, falsificate

Stessa scala, stesso giorno, controllo nudo in ogni ripetizione, 540 Mbit/s
offerti:

| arm | consegnato (entrambi i campioni) | CPU processo |
|---|---:|---:|
| base — 1 flusso, 1 coda TUN, 1 carrier | 435,7 / 434,8 | 15,7–18,6 % |
| 4 flussi interni, 1 coda TUN | 407,9 / 422,4 | 19,8–19,9 % |
| 4 flussi interni, **4 code TUN** | 415,5 / 417,3 | 21,9–22,5 % |
| 4 flussi interni, **4 carrier QUIC** | 390,3 / 359,0 | 27,2–34,5 % |

**Quattro task di uplink su quattro code TUN non consegnano più di uno**, quindi
il ciclo strettamente seriale leggi-batch-poi-invia-batch **non** è il muro.
Quattro carrier consegnano **meno** a quasi il doppio della CPU — l'invariante
`--carriers` esistente, misurata invece che assunta. Approfondire la coda del
dispositivo TUN (`txqueuelen` 500) non alza il tetto: sposta *dove* cadono i
pacchetti, non *quanti* arrivano, che è la firma di una coda drop-tail davanti
a uno scarico a rate fisso.

### Il muro è la coda, e la curva si rovescia

540 Mbit/s offerti, 2 ripetizioni, rtt campionata **dentro** la stessa
trasmissione:

| buffer datagram | consegnato | rtt media | rtt minima |
|---|---:|---:|---:|
| 1 MiB | 389,2 / 388,4 | — | — |
| **8 MiB (default)** | 388,3 / 402,1 | 227 ms | 21 ms |
| 16 MiB | 431,7 / 445,4 | 427 ms | 23 ms |
| 32 MiB | 462,0 / 452,6 | 708 ms | 25 ms |
| 64 MiB | **409,8 / 428,4 ↓** | **1092 ms** | 23 ms |

Tre letture, e la terza decide il default.

* **Il buffer è sul percorso critico**, che §3 (V-3) aveva concluso di no.
  Entrambe le misure sono giuste e la differenza è lo *strumento*: V-3 usava un
  flusso TCP interno, che si autopacizza sulla finestra di congestione e quindi
  non riempie mai il buffer. Un'offerta UDP fissa non arretra. **Una manopola
  può essere invisibile sotto un carico e portante sotto un altro**, e «l'abbiamo
  già misurata» non è la stessa cosa di «l'abbiamo misurata».
* **La curva si rovescia.** 64 MiB è peggio di 32 sulla banda *e* peggio sulla
  latenza. Non è «più profondo è meglio»: c'è un ottimo, e oltre quello la coda
  in più compra solo ritardo.
* **La latenza cresce più in fretta della banda.** Da 8 a 32 MiB la banda
  guadagna ~16 % e la rtt sotto carico triplica. La rtt **minima** resta 21–25 ms
  a ogni gradino: sono millisecondi di *coda stazionaria*, non di percorso
  peggiore — esattamente la grandezza che V-10 aveva appena tolto.

### La decisione: il default resta 8 MiB

La grandezza che conta per una VPN è la banda del TCP **interno**, che vale
`finestra / rtt`. Quindi lo scambio va prezzato nella valuta in cui l'utente lo
paga davvero. Rimisurato con un flusso TCP interno, 3 ripetizioni interlacciate,
banda e rtt sotto carico campionate nella **stessa** trasmissione (controllo
nudo 400,8 Mbit/s quel pomeriggio):

| buffer | upload | % del nudo | rtt sotto carico |
|---|---:|---:|---:|
| 32 MiB | 247,88 Mbit/s | 61,8 % | 141,3 ms |
| **8 MiB (default)** | 240,55 | 60,0 % | 129,5 ms |
| 2 MiB | 238,26 | 59,4 % | 129,0 ms |

**+3 % di banda per +12 ms di latenza.** Sul carico che conta il gradino da
32 MiB non è una vittoria, e sul carico dove *lo è* costa mezzo secondo di coda
stazionaria. Il default resta dov'è — ora perché è stato misurato anche al
gradino profondo, non perché il gradino profondo non fosse mai stato provato.

Quello che **è** cambiato è l'escursione della manopola.
`DIRECT_DATAGRAM_SEND_BUFFER_MAX` era uguale al default, quindi
`BORE_DIRECT_DGRAM_SEND_BUF` poteva solo abbassare e questa domanda non si
poteva porre senza editare il sorgente — che è il posto sbagliato dove far
vivere un esperimento. Ora è 64 MiB: chi spinge UDP di massa dentro il tunnel e
non ha bisogno di latenza interattiva ha il gradino, e la tabella qui sopra dice
esattamente quanto costa.

### Conseguenza

**Non esiste un «tetto a ~400 Mbit/s».** Il tunnel non si ferma a 400: consegna
quello che la profondità della coda gli lascia consegnare, e il listino è la
tabella qui sopra. Il divario residuo rispetto al nudo è un divario di
**latenza**, non di capacità — il che lo mette nello stesso conto di V-10 e
rende il prossimo esperimento utile un esperimento di latenza, non l'ennesima
scala di throughput.

Un limite onesto su tutto questo: in ogni braccio il controller era `bbr` e il
percorso aveva `lost_pct = 0,00`. Stessa riserva di V-12, stesso rimedio —
si riapre con un percorso **lossy** nella matrice.
