# Tunnel segreti — revisione di performance e traversal, 2026-09-11

> Documento finale della campagna sui **tunnel segreti** (`bore local
> --tcp-secret-id` + `bore proxy`), la sola forma di tunnel di bore il cui
> percorso veloce è **peer-to-peer**: il braccio relay è
> `consumer → server → provider`, il braccio diretto è una connessione QUIC
> bucata (`hole-punched`) `consumer ↔ provider` che il server si limita a
> intermediare.
>
> Le prove grezze stanno in
> [`SECRET_STAGING_EVIDENCE_2026-09-11.md`](SECRET_STAGING_EVIDENCE_2026-09-11.md);
> la meccanica del traversal sta in
> [`../nat/NAT_TRAVERSAL.md`](../nat/NAT_TRAVERSAL.md). Qui si tirano le somme:
> cosa è stato misurato, cosa era rotto, cosa è stato cambiato, e come si
> colloca bore rispetto allo stato dell'arte.

## 0. Come rifare tutto fra un mese

Tre comandi e un file di coordinate. Nessuno script in questa campagna contiene
un host, una porta o una credenziale: tutto arriva da `env.sh`
(`$BORE_PERF_ENV`, poi `~/.config/bore-perf/env.sh`, poi `./env.sh`; `chmod
600`, mai nel repository — il modello è `scripts/perf/staging/env.sh.example`).

```shell
# 1. il cancello locale delle risorse (nessun sudo, nessuna porta host)
scripts/perf/secret_leak_hunt.sh            # tutti i bracci

# 2. i cancelli di rete in netns (uno alla volta, MAI due insieme)
sudo -n /abs/path/scripts/udp_nat_netns_test.sh
sudo -n /abs/path/scripts/secret_netns_test.sh

# 3. la campagna su staging, per topologia
TOPO=vm-ws N=20       scripts/perf/staging/sec/sec_ttd.sh   | tee out/sec/s2-vm-ws.txt
TOPO=vm-ws PAIRS=5    scripts/perf/staging/sec/sec_ab.sh    | tee out/sec/s1-vm-ws.txt
TOPO=vm-ws GIB=2      scripts/perf/staging/sec/sec_eff.sh   | tee out/sec/s3-vm-ws.txt
TOPO=vm-ws PROBES=100 scripts/perf/staging/sec/sec_lat.sh   | tee out/sec/s4-vm-ws.txt
TOPO=vm-vm ACK=10     scripts/perf/staging/sec/sec_ack.sh   | tee out/sec/s5-vm-vm.txt
```

`TOPO` è la prima cosa da decidere perché cambia **cosa** si sta misurando:

| `TOPO` | provider | consumer | misura |
|---|---|---|---|
| `vm-ws` | VM di test | workstation | scaricamento da un provider in regione verso un NAT domestico — la forma d'uso comune |
| `ws-vm` | workstation | VM di test | il gemello asimmetrico: il NAT domestico sta dal lato che invia |
| `vm-vm` | VM di test | VM di test | CONTROLLO: nessuna WAN fra i peer, quindi il braccio diretto isola il costo CPU di QUIC da ogni effetto di rete |

Tre regole di casa che questa campagna rispetta e che vanno rispettate anche
alla prossima esecuzione:

* **mai `pkill bore`.** Il deployment porta i tunnel veri dell'operatore. I
  processi locali si uccidono per PID, quelli remoti per `--tcp-secret-id <id>`,
  dove l'id è coniato per esecuzione e non esiste altrove sulla macchina;
* **mai due harness netns insieme** (condividono i nomi `ns0`/`ns1`/`ns2`);
* **niente compilazioni locali mentre un braccio di throughput gira.** La metà
  workstation di ogni misura compete con `cargo` per la CPU, e il braccio
  diretto — che cifra in user space — ne risente più del braccio relay, che
  è TCP nel kernel. È un errore commesso una volta in questa campagna e
  costato la ri-esecuzione di uno stadio.

## 1. Il difetto principale: il listener teneva il socket mentre il dialer stava già chiamando (S-5)

Questa è la scoperta che giustifica da sola la campagna, e non sarebbe emersa da
un test di correttezza: **il traversal funzionava sempre**. 20 tunnel su 20
diretti su tutte e tre le topologie, zero fallback, zero fallimenti. Ciò che
non funzionava era *quanto ci metteva*.

`direct_ready_ms`, letto dal log del consumer su 27 stabilimenti `vm-ws`:

```
37 40 41 42 42 43 43 44 44 48 49 49 50 52 52 52 52 53      <- 18 esecuzioni
1036 1043 1043 1044 1044 1045 1050 1052 1162               <-  9 esecuzioni
```

Bimodale, con niente in mezzo. Una distribuzione con un buco così non è mai la
rete: è un timer. Il timer è il PTO iniziale di quinn — `333 ms + 4 × 166 ms =
999 ms` con l'`initial_rtt` di default della RFC 9002 — e il pacchetto che lo
subisce è il **primo Initial QUIC**, perso perché dall'altra parte, in quel
momento, non c'è ancora un endpoint QUIC.

Perché non c'è: il listener consegna il socket a QUIC **solo dopo** il giro di
check, e il suo giro finiva a secco. Il dialer nomina in ~210 ms e *disabilita
il proprio responder* («i frame in ritardo si contano, non si rispondono»: è il
contratto del giro), mentre il listener — il cui piano adattivo sonda per primi
i candidati *locali* del peer e raggiunge il gruppo riflessivo 150 ms più tardi —
si ritrova a interrogare un indirizzo che ha già smesso di rispondere.

La precondizione è contabile nei log: **8 giri su 52** lato VM finivano
`nominated=None`, contro **0 su 22** lato workstation. Stessa asimmetria dello
stallo.

**La correzione**: un listener che ha appena risposto a una richiesta
autenticata chiude lì il proprio giro. Quel peer ha la chiave, è su questa
generazione, gioca il ruolo opposto e ci raggiunge: è tutto quello che la metà
listener del giro può dimostrare, e la risposta appena inviata è esattamente ciò
che farà partire la chiamata. Restare nel giro non migliora il percorso, tiene
solo il socket lontano da QUIC nel momento sbagliato.

Due dettagli portano il peso, ed entrambi sono documentati in
`NAT_TRAVERSAL.md` §21: la risposta va sul filo **prima** che il giro venga
smontato (finire il giro ferma l'attore che la spedirebbe), e `nominated` viene
valorizzato perché è ciò che disattiva l'escape spray della Fase 7 — spendere
sei secondi di spray per un peer che ci ha appena raggiunto sarebbe lo stesso
errore in formato più grande.

## 2. Gli altri tre interventi

| id | cosa | perché |
|---|---|---|
| **S-7** | `initial_rtt` del percorso diretto: 333 ms → **100 ms** (`BORE_DIRECT_QUIC_INITIAL_RTT_MS`, clamp [10, 333]) | un endpoint diretto di bore non è mai «freddo»: nasce solo dopo uno scambio di check autenticato con quel peer o una connessione TCP di controllo verso quell'host. Sottostimare costa **un** Initial duplicato; sovrastimare costa un PTO intero di silenzio proprio sul pacchetto con la probabilità di perdita più alta della connessione |
| **S-8** | cap del backoff di upgrade relay→diretto: 256 s → **60 s** | il cap *è* il tempo peggiore in cui un tunnel resta sul relay dopo che il percorso diretto è tornato possibile. Il VPN dello stesso codice riprova su una griglia **fissa** da 30 s, in produzione: un tunnel segreto che riprova a metà di quel ritmo è il più conservativo dei due, non un rischio nuovo |
| **S-9** | `pair_cache` limitata (sweep in `remember` + tetto 256 voci) | la scadenza girava solo dentro `recall` e solo per la chiave richiamata: una chiave mai più richiamata non veniva mai più esaminata. Una voce per id di tunnel, per tutta la vita del processo — piccola abbastanza da non farsi notare mai |

A questi si aggiungono i tre difetti trovati prima della campagna su staging e
già documentati nell'evidence: **S-2** (un upgrade relay→diretto vivo non veniva
mai riportato, quindi `current_path` mentiva per il resto della sessione),
**S-3** (l'escape spray abbandonava il socket al primo errore ICMP — difetto
visibile solo su Windows, per una proprietà del kernel) e **S-4** (il test
dell'escape spray sparava sugli altri test).

## 3. I numeri

Tutti i numeri di questo capitolo vengono dalla campagna su staging con il
binario **precedente** alle correzioni (`1bb3243a`): sono la base di confronto,
e sono anche il posto in cui due dei quattro difetti si vedono mentre fanno
danno invece che mentre esistono. Le tabelle grezze stanno in
[`SECRET_STAGING_EVIDENCE_2026-09-11.md`](SECRET_STAGING_EVIDENCE_2026-09-11.md)
§8.

### 3.1 Il traversal funziona: 60 tunnel su 60 diretti

20 tunnel indipendenti per topologia, ciascuno acceso, sollecitato con una
connessione, misurato e spento.

| topologia | diretti | relay | falliti | ttd mediano | min | max |
|---|---|---|---|---|---|---|
| `vm-ws` | 20 | 0 | 0 | 107 ms | 87 | **1150** |
| `ws-vm` | 20 | 0 | 0 | 104 ms | 83 | 110 |
| `vm-vm` | 20 | 0 | 0 | 96 ms | 82 | 452 |

Zero fallback, zero fallimenti, attraverso un NAT domestico, una VM in regione
e in entrambe le direzioni. **La correttezza del traversal non è mai stata in
discussione in questa campagna**, ed è esattamente per questo che serviva
misurare i tempi: un difetto che non fa fallire nulla non ha modo di emergere
da una suite di correttezza.

La colonna `max` è il difetto S-5 visto dall'esterno. Le distribuzioni intere:

```
ws-vm   83  83  85  94  96  97  97  97  98 104 104 104 105 106 106 106 106 107 108 110
vm-ws   87  88  91  95 102 103 104 104 106 107 107 107 112 788 790 792 800 804 1149 1150
vm-vm   82  82  83  85  88  93  93  94  94  96  96  98  98  98 105 106 110 200 205 452
```

`ws-vm` sta tutta in 27 ms. `vm-ws` ha gli stessi 13 campioni in quella banda e
poi **7 su 20 — il 35 % — a 788 ms o peggio**, senza niente in mezzo. Il
discriminante non è la direzione, non è il NAT e non è la tratta WAN: è **quale
host faceva da Listener** (cioè quale host ospitava il provider). Con la
workstation listener, 0 lenti su 20; con la VM listener, 7 su 20 in `vm-ws` e 3
su 20 in `vm-vm` — e `vm-vm` esclude la WAN, perché fra quei due peer non c'è
WAN.

### 3.2 Relay contro diretto: a decidere è l'host che INVIA

Mediana dei rapporti diretto/relay, 5 coppie per cella, 128 MiB su 4
connessioni per braccio, ordine alternato dentro la coppia.

| topologia | verso | host che INVIA | mediana diretto/relay |
|---|---|---|---|
| `vm-ws` | get | VM | **1.218** |
| `vm-ws` | put | workstation | 0.757 |
| `ws-vm` | get | workstation | 0.828 |
| `ws-vm` | put | VM | 1.167 |
| `vm-vm` | get | VM | 2.298 |
| `vm-vm` | put | VM | 1.456 |

Letta per direzione o per topologia, questa tabella sembra rumore. Letta per la
terza colonna, non lo è più:

```
mittente = VM             1.218   1.167   2.298   1.456    -> il diretto vince, sempre
mittente = workstation    0.757   0.828                    -> il diretto perde, sempre
```

**Il merito relativo del percorso diretto è una proprietà dell'host che INVIA**,
non della direzione né della topologia. La workstation invia QUIC più
lentamente di quanto invii TCP; la VM no. La spiegazione candidata è la CPU: il
percorso diretto cifra e paced-invia in user space, mentre il braccio relay è
TCP nel kernel con segmentation offload. I buffer socket sono esclusi come
spiegazione (`net.core.rmem_max`/`wmem_max` valgono 4 MiB su tutti e tre gli
host); la verifica è S3, che misura secondi di CPU per GiB consegnato.

Una precisazione onesta sul controllo `vm-vm`: **non** isola il costo CPU di
QUIC, malgrado quanto diceva il commento dell'harness. Il suo braccio diretto è
loopback mentre il suo braccio relay attraversa comunque due volte la WAN fino
al broker, quindi 2.298 è un effetto di topologia. Quello che stabilisce è il
**tetto** del percorso diretto su quell'host: 387 MB/s.

### 3.3 Quanto costa ciascun trasporto — e la sola affermazione che conta

`vm-vm`, 2 GiB per braccio:

```
relay    204.03 MB/s  path=relay   relay_tx=2148532224  relay_rx=68
direct   407.83 MB/s  path=direct  relay_tx=0           relay_rx=0
```

`relay_tx` è il contatore di byte **del server** per quel tunnel. Sul braccio
relay vale 2 GiB più framing: ogni byte consegnato è passato dal broker. Sul
braccio diretto vale **zero**: il server ha intermediato la bucatura e poi non
ha portato niente.

È la differenza strutturale fra il tunnel segreto e ogni altro trasporto di
bore — nelle campagne vhost e public il braccio «diretto» terminava comunque
sul server, che pagava i byte in entrambi i casi — ed è detta in una forma
falsificabile invece che retorica: se il percorso diretto non fosse davvero
peer-to-peer, quel contatore non sarebbe zero.

### 3.4 Latenza di nuova connessione e scala

100 sonde per gradino, gradini a 0/16/64/256 connessioni tenute aperte, p50 in
millisecondi.

| topologia | trasporto | held=0 | 16 | 64 | 256 |
|---|---|---|---|---|---|
| `vm-ws` | relay | 26.99 | 26.71 | 26.87 | 26.80 |
| `vm-ws` | diretto | *(persa: S-5)* | 21.70 | 21.75 | 21.63 |
| `vm-vm` | relay | 3.33 | 3.43 | 3.46 | 3.31 |
| `vm-vm` | diretto | 0.745 | 0.767 | 0.805 | 0.776 |

Una connessione nuova costa **21.7 ms contro 26.9 ms** sulla tratta domestica e
**0.78 ms contro 3.4 ms** con entrambi i peer in regione — il transito in più
del relay è tutta la differenza. E **nessuno dei due trasporti degrada con la
concorrenza**: 256 connessioni tenute aperte spostano la p50 di meno di 0.2 ms
su ogni riga. È il comportamento che il semaforo `--max-conns` e il modello «una
connessione proxata = uno stream QUIC» devono dare, e qui — a differenza della
coda di concorrenza che la campagna vhost non ha mai chiuso (N-9) — lo danno.

## 4. Quello che ha rotto l'harness, non il prodotto

Quattro difetti dell'harness, tutti trovati eseguendolo e tutti della stessa
famiglia: **una misura sbagliata che ha la forma di una misura giusta**. Sono
documentati per esteso nell'evidence (§3.1-§3.3); qui contano perché chi
rifarà questi test fra un mese li incontrerebbe di nuovo.

| id | cosa | come si manifestava |
|---|---|---|
| **H-12** | il controllo «origine già avviata?» era un `pgrep` remoto, che combacia con la propria riga di comando | l'origine non partiva mai, S1 misurava **0.00 MB/s su entrambi i bracci** |
| **H-13** | un braccio fallito entrava nella mediana come rapporto 0 | `s1-vm-ws get` riportava 0.965 con due righe `startfail`; i tre bracci reali erano 0.965/1.247/1.218, mediana vera **1.218** |
| **H-14** | `GIB` non intero non fermava lo script | riga stampata con MB/s vuoto e finestra `t0 == t1`, cioè una divisione per zero a valle |
| **H-15** | lo stadio di efficienza apriva la finestra prima che il percorso esistesse | S-5 si è mangiato il braccio diretto di S3 `vm-ws` e il primo gradino di S4 `vm-ws` (`errs=100`) |

Lo zero di H-13 merita una riga in più, perché non è «rumore»: uno zero può
solo essere il minimo della lista, quindi l'errore è **orientato**. Ha
trasformato «il diretto vince del 22 %» in «il diretto perde del 3.5 %».

La regola che ne esce, e che i tre stadi ora applicano allo stesso modo: un
braccio entra in una mediana solo se ha misurato qualcosa, la riga esclusa
resta **stampata** con `-` al posto del rapporto, e lo stadio dichiara quante
coppie ha escluso. Un'esclusione invisibile è solo un secondo modo di
nascondere un fallimento.

## 5. Confronto con lo stato dell'arte

Il confronto va fatto per **meccanismo**, non per slogan, perché è l'unico modo
per dire cosa manca davvero.

| meccanismo | bore | Tailscale | frp (`xtcp`) |
|---|---|---|---|
| scoperta candidati STUN | sì, catena di 2+ server con budget | sì, STUN in ogni regione DERP | sì, un server STUN |
| classificazione NAT (EIM/EDM, port-preserving) | sì, profilo strutturato sulle offerte | sì (netcheck) | no |
| piano adattivo calcolato dal broker | sì, e solo quando entrambi i profili esistono | sì, lato client | no |
| check di connettività **autenticati** (HMAC) | sì, richiesta==risposta, mai rispondere a non autenticati | sì (DISCO) | no |
| candidati peer-reflexive appresi | sì | sì | no |
| birthday paradox per NAT simmetrici | sì (Fase 7) | sì | no |
| port mapping gestito (PCP/UPnP) con rinnovo | sì (`--upnp`, lease PCP RFC 6887 con rilevamento del riavvio del gateway) | sì | no |
| relay sempre caldo, fallback per connessione | sì | sì (DERP) | fallback a `stcp` con timeout |
| upgrade relay→diretto su sessione viva | sì, backoff 2…60 s | sì, continuo | no |
| cache della coppia vincente | sì, TTL 120 s, invalidata al primo fallimento | sì | no |
| **IPv6** | **no** | sì | no |

Due letture di questa tabella.

La prima: rispetto a frp — il termine di paragone più citato nel mondo dei
tunnel self-hosted — non c'è partita, e non per una questione di qualità di
implementazione ma di meccanismi presenti. frp fa STUN e buca; se il NAT è
simmetrico, ripiega. bore classifica il NAT, ordina le sonde di conseguenza,
impara i candidati peer-reflexive, e sul caso duro tira il birthday paradox.

La seconda, più utile: rispetto a Tailscale il divario residuo è **uno solo**,
ed è IPv6. Tailscale dichiara ~94% di connessioni dirette, e una parte
sostanziale di quel numero non viene dal bucare NAT: viene dal non doverlo fare,
perché i due peer hanno indirizzi IPv6 globali e si parlano direttamente. Ogni
altro meccanismo della colonna è presente in bore, in alcuni casi con
raffinatezze che Tailscale non documenta (il jitter deterministico legato al
ruolo, che rompe il lockstep del conntrack sui router che mascherano — misurato
in pcap, non ipotizzato).

**Il gap IPv6 resta aperto e va dichiarato tale.** È una feature, non
un'ottimizzazione: tocca il modello dei candidati (che oggi sanitizza e scarta
gli indirizzi non-IPv4), il wire delle offerte, la classificazione del profilo e
il dual-stack del socket di punch. Non è stato fatto in questa campagna e non
andrebbe fatto di corsa: è il prossimo pezzo di lavoro, con il suo piano.

