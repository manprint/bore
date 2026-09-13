# bore — relazione finale della campagna cablata, 12–13 settembre 2026

> **Stato: IN CORSO.** Le sezioni marcate *(in attesa)* si riempiono quando la
> fase corrispondente chiude. Le sezioni senza marcatura sono chiuse e non si
> riaprono senza nuove misure.

---

## 0. Perché questa campagna esiste

Ogni campagna precedente che ha usato la workstation come estremo di traffico è
stata misurata **in WiFi**. Cablata, la stessa linea dà **925–933 Mbit/s in
download e ~740 in upload**. Le campagne avevano registrato 369–416 in download,
e `vpn_direct_deficit.sh` citava **150,7**.

Il download era quindi sottostimato fra **2,3× e 6×**, e ogni conclusione tratta
su quella direzione descriveva la radio, non il prodotto. Questa campagna
rimisura tutto su rame e separa le due cose.

**Obiettivo costante:** massima banda, minima latenza, massima stabilità. Per il
jump host l'obiettivo è esplicitamente la **latenza**.

---

## 1. La linea, e la prova che è la linea giusta

Nessun valore assoluto vale se non si sa a chi appartiene il tetto. La risposta
è misurata, non assunta (`scripts/perf/staging/asym_qualify.sh`): la stessa
offerta verso **tre destinazioni indipendenti** legge lo stesso numero, e i
contatori di allowance ENA della VM restano **a zero prima e dopo ogni arm**.

| destinazione | upload P=8 (12/09) | upload P=8 (13/09) | download P=8 (12/09) | download P=8 (13/09) |
|---|---|---|---|---|
| VM di test (AWS Milano) | 740 | 656 | 933 | 934 |
| provider francese non correlato | 730 | 611 | 917 | 910 |
| Cloudflare (HTTPS) | 779 | 725 | — | — |

Il download è identico a un giorno di distanza; l'upload scende verso **tutte e
tre** — vedi §1.1.

Un limite che segue la **sorgente** legge uguale verso tutte; uno che appartiene
a una destinazione no. Legge uguale verso tutte: il tetto è **di questa
workstation**, ed è il riferimento corretto per ogni "percento del nudo" in
questo repository.

A **P=1** la stessa misura legge 726 / 583 / 210, ordinata per RTT: è
`finestra / RTT`, non un policer. Quindi ogni percentuale a **connessione
singola** cita Mathis prima di citare bore.

### 1.1 Ma le due direzioni non sono ugualmente citabili, e questo è nuovo

Quattordici letture della linea nuda in nove ore, una prima di ogni blocco:

- **download 922–929 Mbit/s, dispersione 0,8 %** — fermo;
- **upload 622–742, dispersione 19,3 %** — e non è una tendenza. Le tre letture
  basse delle 04:25–04:30 sembravano un calo dell'uplink; **quattro minuti
  dopo** la lettura successiva è tornata a 740. È un'**oscillazione** su scala
  di minuti.

La conferma con un secondo strumento sta dentro `asym_qualify` stesso: le **due
ripetizioni della stessa cella**, a due minuti di distanza, leggono **617** e
**739** Mbit/s in upload mentre il download delle stesse due legge 923 e 925. E
la ri-esecuzione del 13/09 legge 656 / 611 / 725 verso le tre destinazioni dove
il 12/09 leggeva 740 / 730 / 779: scende verso **tutte**, quindi è della
sorgente. Quel 740 non era un valore stabile, era un **campione alto**.

**Conseguenza operativa, che vale per chiunque rifaccia questi test:** le
percentuali di **download** di questo documento poggiano su una linea che si
muove dello 0,8 %; quelle di **upload** vanno lette solo come **rapporti contro
un controllo nudo campionato nella stessa ripetizione** (V-9), e una fase di
upload a una sola ripetizione su questa linea campiona la fase dell'oscillazione,
non il prodotto. Dettagli e tabelle: §38 delle evidenze.

---

## 2. Risultato per modo

| modo | rispetto al nudo | nota |
|---|---|---|
| `bore transfer` | **99,2 %** | allo stato dell'arte, §23 delle evidenze |
| vhost, banda | **93–101 %** della linea | 20 punti di download su 21 fra 857 e 934 Mbit/s, su **sette** configurazioni (relay e diretto, 1/4/8/auto carrier). Non c'è piu' un deficit da spiegare — §39 |
| vhost, 8 richieste concorrenti | relay **408–430 rps / p50 19,2 ms**; diretto a 1 carrier **361 / 22,1** | il costo del percorso diretto qui è la **latenza**, e `--carriers 4` la recupera (397 / 19,5). Il relay ha lo stesso p50 a uno e a otto carrier — §39.3 |
| secret | **92–94 %** | e regge sotto concorrenza, +0,2 ms |
| VPN diretto | **94,2 % su / 89,3 % giù** | il "deficit del 37 %" era la radio |
| VPN relay | **50 % giù / 63 % su** | 467/474 Mbit/s contro 933/748, a **~1 core intero**. Il braccio di controllo (doppio transito senza bore) è bloccato dai security group: il rapporto è pubblicato, la separazione fra deployment e codice **no** — §28 |
| public, 1–2 connessioni | **96,8–98,3 %** della linea (relay) | e il percorso diretto non serve a questo: 0,96–0,98 del relay. A connessione singola i due trasporti sono pari entro l'**1 ‰** (0,9995) — §35 |
| public, 4+ connessioni | **non citabile** | la cella ripetuta ha un'escursione del **38 %** a n≥4 (contro il 2–6 % a n=1 e n=2), quindi la separazione fra i due bracci su cui §36 concludeva non regge alla replica: rimisurate, le due celle si sovrappongono su quasi tutto l'intervallo. §36 è **ritirato** e con esso la sua correzione. Il ladder pubblico va citato **fino a n=2**. Perché la varianza compaia a n≥4 resta aperto, con tre candidati non separati — §42 |
| public, asimmetria | download/upload **1,116** (appaiato) | l'asimmetria del tunnel era **della radio**: in WiFi 0,38, cablata 1,116 — invertita, non ridotta. L'allowance in **uscita** dell'istanza è assolta da sei bracci con delta zero. Le percentuali sulla linea restano **provvisorie** finché `asym_qualify` non gira in questa finestra — §34 |
| jump host, apertura sessione | **250 ms** di handshake esterno + canale (era 1249) | di cui **~46 ms sono bore**: il resto è l'handshake SSH imposto da RFC 4253/4252. Il secondo che c'era prima era un `sleep` di russh, corretto — §44.2, §44.5 |
| jump host, tasto a sessione aperta | `chan` **87–89 ms**, `echo` **129–130 ms** | due attraversate della forcina a due host; il termine di bore non è misurabile a questa scala |
| jump host, relay contro diretto | **indistinguibili** (0,4 % contro ±14 % di dispersione) | e i carrier non costano latenza. Per un jump host **`--udp` non serve** alla latenza — §44.6 |
| jump host, isolamento sotto carico | **0,97× / 1,00×** | 690 MiB su un secondo canale non si sentono sull'interattivo: prima verifica sul campo della correzione HOL di russh — §44.7 |
| jump host, stabilità | **16 PASS / 0 FAIL / 0 SKIP** | caduta sul relay in 11 s, ritorno al diretto in 15–16 s, 5 rekey attraversati, una sola riga admin — §44.8 |
| VPN, cablaggio delle manopole | **5/5** al device, **9/9** sulle rotte | `txqueuelen` e MTU riletti dal kernel; default-deny delle rotte verificato in nove combinazioni — §45.1, §45.2 |
| VPN, tempo di buio se il diretto muore | **13,1 s** di default | e la coppia idle/keepalive lo **controlla** (4,7 s / 40,8 s ai due estremi); l'RTT iniziale no. Riproducibile allo **0,07 %** fra due esecuzioni — §45.4 |
| VPN, `--carriers` sul **diretto** | **neutri**, 0,948 contro 0,954, a **~95 %** della linea | con 4 e 8 flussi non c'è margine da recuperare — §45.5 |
| VPN, `--carriers` sul **relay** | **dannosi** a flusso singolo: 0,824 → 0,286 / 0,440 | insiemi **non sovrapposti**; è BW-F2 (round-robin per datagramma) sul percorso dove non è stato corretto. A 4–8 flussi **non citabile** — §45.5 |

---

## 3. Difetti trovati — prodotto

1. **M-1, perdita della connessione di controllo.** Una TCP `ESTABLISHED` per
   ogni riconnessione del connector VPN, a **entrambi** gli estremi, mai
   riassorbita. Causa: il driver yamux gira in un task staccato e usciva solo
   quando chiudeva il *peer* — e il server esegue lo stesso driver, quindi
   nessuno dei due iniziava mai. Corretto contando i riferimenti vivi (opener,
   acceptor **e ogni substream consegnato**): a zero il driver chiude. Contare i
   substream è ciò che rende la correzione sicura — sopravvivono all'opener.
   Cancelli: quattro test in `src/mux.rs`, ognuno rifiuta una correzione
   sbagliata diversa; red-check = il primo va in timeout (il sintomo di
   produzione) senza il conteggio.

   **CHIUSO e verificato su tre livelli il 13/09.** Unit verdi; gate netns
   `T-CTRLLEAK` verde dentro una suite di **169 PASS / 0 FAIL / 1 SKIP**
   (lo skip è `T-PINMTU`, che si dichiara non discriminante invece di tacere);
   e la stessa fase sul percorso reale, contro il server vero, legge
   **1 1 1 1** attraverso quattro riconnessioni e **1** dopo 120 s di
   assestamento, dove prima leggeva **1 2 3 4 5** e nessuna riassorbita. Il
   file del «prima» è conservato accanto a quello del «dopo»: un «dopo» da
   solo non dimostra una correzione — §43.
2. **V-14a, scritture del relay coalescenti.** Una `write_all` per pacchetto era
   un header yamux e un giro di driver per pacchetto — ~52 000 al secondo a
   1350 byte di MTU e 570 Mbit/s. Ora la coda si drena con `recv_many`. Il
   formato non cambia di un byte.
3. **I-SSH12: l'82 % dell'apertura di una sessione SSH era un `sleep`.**
   `russh::server::Config::auth_rejection_time` vale **1 s** per difetto e
   `auth_rejection_time_initial` vale `None`, che **ricade su quello**; bore non
   impostava né l'uno né l'altro. Ogni client OpenSSH apre con una richiesta
   `none`, che RFC 4252 §5.2 rende la **sonda di enumerazione dei metodi**: il
   rifiuto del server è ciò che porta al client l'elenco dei metodi. Misurato
   con `ssh -v` marcato per riga sul percorso reale a 24 ms di RTT: key exchange
   chiuso a 0,133 s, elenco dei metodi a **1,155 s**, canale aperto a 1,202 s,
   banner interno a 1,252 s — **1,022 s in un solo intervallo**, contro ~46 ms
   di lavoro vero del gateway. Pagato da **ogni** sessione di **ogni** ingresso
   (jump, vhost `ssh -R`, public, secret). Corretto con
   `auth_rejection_time_initial: Some(Duration::ZERO)`, lasciando 1 s per una
   credenziale **sbagliata** — ritardare `none` non rallenta nessun attacco
   (non contiene segreti da indovinare, e l'alternativa per chi attacca è una
   TCP nuova più un key exchange completo). Dopo: lo stesso intervallo è **20
   ms**, `wchan` 1248,6 → 250,8 ms, `open` 1592,7 → 657,8 ms. Manopole
   `BORE_SSH_AUTH_REJECT_MS` / `_INITIAL_MS`; il risolutore è puro e tiene il
   default su un valore malformato (zero lì significherebbe «rispondi subito a
   ogni password sbagliata»). Cancelli: il test di cablaggio ora fissa il campo
   — red-check `left: None`, esattamente il difetto di produzione — più quattro
   unit sul risolutore. §44.2.
4. **V-14b, AEAD del relay in una allocazione e una copia.** Erano tre per
   sigillo e due per apertura, su ~53 000 pacchetti/s per direzione su entrambi
   gli estremi. Le funzioni libere restano **come oracolo**: un test confronta
   byte per byte a 0/1/1350/1500/65535 byte e tre contatori.

---

## 4. Difetti trovati — harness

Un harness che sbaglia costa più di un prodotto che sbaglia, perché produce
numeri che sembrano veri.

1. **`sort -n` dipende dal locale (V-11).** Sotto `it_IT.UTF-8`
   {397,46 · 264,01 · 408} si ordina 408 · 264,01 · 397,46 e la mediana legge
   264,01. Ora `med()` confronta i valori **come numeri** in awk: nessun locale
   può reinterpretarli.
2. **Uno zero che significa "lo strumento ha fallito"** — tre volte:
   `cf()` pubblicava `0` per un download Cloudflare che non aveva scaricato
   nulla; `vpn_hub` usciva con `rc=0` senza aver misurato; `vpn_relay_attrib`
   stampava `0.00` per endpoint irraggiungibili. Corretto **alla strozzatura**:
   `tcp_mbps`/`udp_loss` stampano `FAILED` quando iperf3 non ha prodotto JSON
   utilizzabile (uno 0 *misurato* resta 0 — un percorso in blackhole consegna
   davvero 0), e `med()` rifiuta un campione non numerico **dicendolo** su
   stderr. Sette fasi spingevano quello 0 dritto in una mediana.
3. **Una fase più corta della rampa misura la rampa (V-19).** Le fasi pubbliche
   spostavano 96 MiB — due secondi in WiFi, **0,83 s** su rame — e producevano
   rapporti appaiati da 0,623 a 1,323 su arm che differiscono per una variabile.
   La dimensione è ora una variabile (`XFER_MB`, default cablato 384 MiB) e
   ri-derivarla è la prima cosa che una campagna fa quando cambia la linea.
4. **Confrontare la DURATA di una fase con il suo preventivo prima di leggerne
   il risultato.** `vpn_hub` ha chiuso in 19 s contro un `timeout` di 2400: era
   l'unico segnale visibile che non aveva misurato nulla.
5. **Una fase collegata a nessun driver è un cancello che non esiste.**
   `asym_qualify` — la fase che qualifica la linea — non era collegata, e la sua
   risposta è rimasta non letta per un'intera campagna.
6. **Verificare la raggiungibilità dal lato che MISURA, non il bind dal lato che
   SERVE**, e su un host che esegue il prodotto in un container ricordare che
   *il piano di rete dell'host non è il piano di rete del prodotto* (§25.6).
7. **Le coordinate non sfuggono dal codice, sfuggono dalla PROSA.** Il codice
   legge gli indirizzi dall'ambiente per costruzione; un documento che spiega una
   misura cita quello che la misura ha stampato — indirizzi compresi. La
   scansione è stata eseguita una volta sola, prima del commit, e ha trovato
   **tre** riscontri vivi in testo scritto nella stessa sessione. Ora è uno
   script (`secret_scan.sh`) che **deriva** i pattern da `~/.config/bore-perf/env.sh`:
   uno scanner che scrive un indirizzo di staging lo ha pubblicato — cosa che
   quello script ha red-checkato su se stesso, segnalando un indirizzo reale
   finito in un suo commento.

8. **Una domanda del piano senza una fase è una domanda senza risposta, e non
   si vede.** Il piano P6 pone al jump host **cinque** domande. Ne era cablata
   **una sola fase**, che ne copriva quattro; la quinta — *stabilità: rekey
   attraversato, ritorno sul relay warm quando l'UDP muore* — non aveva alcuna
   fase, e la lacuna non era visibile da nessuna parte: il driver girava
   `rc=0`, la fase produceva una tabella completa, e nulla nel repository
   diceva che mancava la metà che importa di più. È lo stesso difetto del
   punto 5 in una forma peggiore: lì una fase esisteva e nessuno la eseguiva;
   qui la fase non esisteva affatto e il risultato sembrava completo.
   Correzione strutturale: `rerun_jump.sh` porta in testa la **mappa
   domanda → fase**, quindi una domanda scoperta è visibile nel file che
   dovrebbe rispondervi. Nuove fasi `jump_stab.sh` (stabilità, fallback,
   riga di admin unica) e `jump_hol.sh` (isolamento fra canali sotto carico —
   la ragione per cui russh è vendorizzato, e finora coperta solo da test in
   processo, che su loopback falsificano proprio questa classe).

9. **Un permesso `sudo` va verificato sul percorso esatto, non letto
   dall'elenco.** Avevo concluso che `scripts/vpn_tun_endpoint.sh` non fosse
   fra i comandi NOPASSWD, e stavo per progettare la fase di stabilità attorno
   a quel vincolo (rinunciando al blackhole, quindi all'esperimento). L'elenco
   di `sudo -n -l` contiene anche una riga glob `scripts/*`, che **copre** quel
   percorso: il glob non attraversa `/`, e lì non ce n'è. `sudo -n -l <percorso
   assoluto>` risponde in una riga e chiude la domanda. Una capacità creduta
   assente costa quanto una creduta presente — nel primo caso si rinuncia
   all'unica misura che rispondeva alla domanda.

10. **Una variabile che nessuno assegna uccide una fase in zero secondi, e né
    `bash -n` né shellcheck la vedono.** `ws_tunnel` è morta con
    `line 19: B: unbound variable`; il driver ha scritto `FAIL ... elapsed=0s`
    ed è andato avanti, e il buco si è visto ore dopo contando i marker. Lo
    stesso `$B` inesistente aveva già colpito due fasi sorelle la stessa sera.
    `bash -n` passa — lo script è sintatticamente perfetto. E SC2154 di
    shellcheck **ignora di proposito i nomi tutti maiuscoli**, perché li assume
    ambientali: qui lo sono tutti. Misurato, non letto sul manuale: un file con
    solo `echo "$UNDEF"` non produce alcun SC2154, lo stesso file con `$undef`
    ne produce uno.

    Quindi il controllo è nostro: `unbound_scan.sh`, eseguito da `lint.sh`,
    red-checkato reintroducendo il bug originale. Alla prima esecuzione pulita
    ha trovato **tre abort latenti veri**: `$K` (un nome di chiave ssh che non
    esiste in nessun posto — `ena_watch.sh` e `server_seq.sh` sarebbero morti al
    primo ssh), `$PRETTY_NAME` letto nudo da `/etc/os-release`, e due contratti
    di libreria (`VPN_LINK_ID`, `JUMP_SERVER_EXTRA`) che funzionavano solo
    perché ogni chiamante si ricordava di impostarli.

    **Il difetto era mio due volte.** La prima versione dello scanner segnalava
    128 file su 132, cioè un cancello che nessuno può passare — la cosa esatta
    contro cui l'intestazione di `lint.sh` mette in guardia. Le cause erano tutte
    mie: `$l` dentro un programma jq fra apici singoli non è una variabile di
    shell, `\$r` dentro doppi apici nemmeno, il corpo di un heredoc quotato
    nemmeno, `local l=$1 s=$2 a b t0 t1` dichiara sei nomi e ne leggevo uno, e
    `t1=$(date) back=""` ne assegna due. Regola: **un linter che segnala tutto
    non segnala nulla**, e la direzione sicura dell'errore è legare troppo (si
    perde una rilevazione) e mai troppo poco (si perde la credibilità).

11. **Un array di fase che oscura uno SCALARE della libreria stampa una
    coordinata nelle evidenze.** `lib.sh` pubblica `S="$BORE_SRV"`, l'indirizzo
    del server di staging; otto fasi dichiaravano la propria tabella di campioni
    come `declare -A S`. Bash **non** azzera uno scalare quando diventa array:
    ne conserva il valore come elemento **[0]**. Quindi tutte e otto stampavano
    l'indirizzo reale del server nel proprio blocco di campioni grezzi, sotto la
    chiave `0`, a ogni esecuzione. Trovato in `pub_ws_conns_r2.out`, fra i
    campioni quic e quelli relay.

    Non è mai entrato in una mediana — `med()` rifiuta un campione non numerico,
    e quella guardia si è ripagata qui — ma è entrato in un **file di evidenza**,
    che è esattamente il modo in cui le coordinate sfuggono in questo progetto:
    dalla prosa e dall'output, mai dal codice. Il rimedio è un **nome**, non una
    disciplina: le tabelle di una fase si chiamano `SAMP`, `R`, `D`, `U`, mai una
    lettera sola che la libreria già possiede. Cancello `shadow_scan.sh`,
    red-checkato.

12. **E `secret_scan.sh` non poteva vederlo: il punto cieco è `out/`.** Quello
    scanner guarda ciò che git porterebbe — il diff più i file non tracciati — e
    `out/` è gitignored, quindi ha detto CLEAN per tutto il tempo. Il nuovo
    `--out` guarda i file di **risultato** (`.out`, `.tsv`, `.md`, `.txt`; i
    `*.log` di bore sono saltati, contengono indirizzi per progetto e
    seppellirebbero l'unica riga che conta). Non fa parte del gate di commit —
    quei file non si committano mai — ma **va eseguito prima di citare un
    risultato in un documento**: è l'unico momento in cui una coordinata passa
    da un file ignorato a uno tracciato, ed è come sono avvenute tutte le fughe
    di questa campagna. Prima esecuzione: 28 righe in 11 file, nessuna arrivata
    in un file tracciato.

13. **Un disegno APPAIATO il cui riepilogo divide le MEDIANE ha buttato via
    l'appaiamento.** Due bracci girano dentro la stessa ripetizione per una
    ragione sola: la deriva si annulla solo se è comune a entrambi.
    `mediana(A)/mediana(B)` la annulla — le due mediane possono venire da
    ripetizioni diverse, quindi il numero pubblicato non appartiene a nessun
    esperimento eseguito. Misurato in `pub/ws_asym.sh`: ha diviso il download
    della rep 1 per l'upload della rep 2, due bracci a cinque minuti di
    distanza, stampando **1,191** dove le ripetizioni intere danno 1,060 /
    1,250 / 1,116, mediana **1,116**. Qui il 7 % non cambia la conclusione; il
    principio sì.

    Corretto: il rapporto si calcola **dentro** la ripetizione, si pubblica la
    mediana di quei rapporti e si stampano tutti (V-11). L'appaiamento è tenuto
    per ripetizione e **mai** per indice negli array dei campioni — `keep`
    scarta un braccio fallito e un indice condiviso farebbe scivolare ogni
    elemento successivo, cioè lo stesso difetto un livello più in basso.
    Red-checkato su tre casi, incluso «la ripetizione di mezzo perde un
    braccio».

14. **Una legenda che nomina una sola tratta di un percorso a due tratte rende
    illeggibili i propri contatori.** `ws_asym.sh` scriveva «download = server
    OUTBOUND», ma il tunnel è un **relay**: ogni byte attraversa il server due
    volte. Così `bw_in_allowance_exceeded` durante un **download** sembrava un
    errore dello strumento e non lo era — incrimina la tratta VM → server di
    quel download. Un delta di allowance nomina una **tratta**, mai una
    direzione.

15. **Una chiamata di livello superiore a una funzione definita PIÙ SOTTO nello
    stesso file è un no-op silenzioso.** bash risolve una chiamata quando la
    riga **gira**: il risultato è `command not found`, uscita 127, su una riga
    che nessuno protegge con `|| fail=1` — perché chiamare una propria funzione
    è l'ultima cosa che ci si aspetta possa fallire. `bash -n` passa. shellcheck
    passa.

    Trovato nel gate di build di questa stessa campagna: `p5_build_gate.sh`
    chiamava `invalidate_binary_gates` alla riga 176 e la definiva alla 203.
    Quella funzione è **l'intero meccanismo** per cui il marker di un cancello
    di prodotto viene invalidato quando il binario cambia — cioè la cosa scritta
    apposta perché la correzione di M-1 venisse davvero riverificata sul
    percorso reale invece di essere saltata con `SKIP (marker present)`. Non
    avrebbe fatto nulla, in silenzio.

    Cancello nuovo: `order_scan.sh`, dentro `lint.sh`, red-checkato su quella
    forma esatta. Ambito volutamente stretto e quindi corretto: solo le
    chiamate di livello superiore, e le stringhe fra apici vengono azzerate
    prima (`trap 'cleanup; assert_clean' EXIT` è l'idioma di ogni fase, e il
    suo corpo si valuta quando il trap scatta). Il red-check della funzione
    stessa, eseguita in isolamento sui tre casi — nessun record precedente,
    binario invariato, binario cambiato — conferma che invalida **solo** il
    verdetto di prodotto e lascia stare le misure.

16. **Il guardiano di contesa dei driver ha rifiutato di partire per colpa di
    un MONITOR che leggeva il suo log.** Quattro driver portavano quattro copie
    dello stesso controllo, e tutte e quattro usavano
    `ps -eo pid,args | awk '$0 ~ /rerun_eth\.sh/`: una regola che trova il nome
    **ovunque** nella riga di comando, in qualunque campo. Misurato il 13/09 con
    P7 finito e nulla in esecuzione: il processo «colpevole» era un `bash -c` il
    cui corpo *nominava* il driver mentre ne seguiva il log. È la terza volta in
    questa campagna che identificare un processo per testo dà una risposta
    sbagliata (un `pkill` che uccise la sessione che lo lanciava; un guardiano
    che trovava la propria sotto-shell di sostituzione di comando).

    Corretto con una regola **sana**: un processo esegue un driver solo se il
    nome del file è uno dei **primi due argomenti** del suo argv, letto da
    `/proc/<pid>/cmdline` (separato da NUL, quindi non ambiguo su dove finisce
    un argomento — a differenza di `ps args`, dove un nome di file e una frase
    che lo cita si somigliano). Un `bash -c '<testo>'` mette il testo da argv[2]
    in poi e non può più corrispondere. Il controllo vive ora in **un** file,
    `driverlib.sh`, sorgente dei quattro driver: un guardiano duplicato quattro
    volte è un difetto da correggere quattro volte. Red-checkato sulle tre forme
    reali di lancio (`./x.sh`, `bash ./x.sh`, `nohup ./x.sh`) più il monitor che
    lo nomina.

    **E la correzione era incompleta: il quinto portatore era rimasto indietro.**
    I quattro driver di campagna sono stati convertiti; `p5_build_gate.sh`, che
    porta la *stessa* guardia per la ragione opposta (rifiutarsi di compilare
    mentre una fase misura, perché la CPU è parte dello strumento), conservava
    la copia vecchia. Chiuso il 13/09 facendogli sorgere `driverlib.sh` come
    agli altri, e red-checkato **dal vivo**: con lo sweep in corso la guardia
    nomina il driver reale (`656688 bash ./scripts/perf/staging/rerun_eth.sh`) e
    nient'altro, mentre la sessione era piena di comandi che scrivevano
    `rerun_eth.sh` nel proprio testo.

    La lezione non è sulla guardia: **una correzione che elimina una duplicazione
    deve contare i duplicati prima di dichiararsi finita.** Quattro su cinque
    letti come «tutti» perché quattro erano il gruppo che si aveva in mente.

17. **Una funzione chiamata in `$( )` non può pubblicare una variabile.** La
    sostituzione di comando gira in una **sotto-shell**: ogni assegnamento fatto
    lì dentro viene scartato all'uscita, e il chiamante legge il valore
    precedente — di norma la stringa vuota — stampando il suo default per
    sempre, senza niente nell'output che dica che un valore è andato perso.

    Preso in `pub/ws_conns_procs.sh` **prima** della sua prima esecuzione vera:
    `cell()` stampava il rate principale su stdout e impostava `CELL_WALL` per
    la colonna di riscontro. Scritta così, quella colonna avrebbe letto `n/a` in
    ogni cella di ogni esecuzione — e `n/a` in una colonna a cui `n/a` **è
    concesso** è indistinguibile da una cella che davvero non aveva la seconda
    lettura. Corretto facendo viaggiare entrambi i numeri su stdout, che è la
    forma che non può marcire: il prossimo autore non può reintrodurre il difetto
    aggiungendo un `$( )`, perché c'è già.

18. **Un marker di ripresa sopravviveva alla modifica del codice che attestava.**
    La finestra di build aveva già la regola giusta per il BINARIO — un verdetto
    di prodotto vale solo per il binario che l'ha prodotto, e al cambio di
    checksum il marker viene invalidato. La stessa regola mancava un livello più
    su, per i propri passi: `fmt`, `clippy_*` e `test_*` attestano qualcosa su
    `src/` e `tests/`, e i loro marker sopravvivevano a una modifica dell'uno o
    dell'altro.

    **Misurato mentre girava.** Tre invocazioni consecutive hanno corretto un
    errore di clippy, poi uno unit test, poi un test e2e; la quarta ha stampato
    `SKIP fmt`, `SKIP clippy_default`, `SKIP clippy_vpn`, `SKIP clippy_jump`
    benché `src/secret.rs` e `tests/e2e_test.rs` fossero stati modificati DOPO
    la scrittura di quei marker. La finestra si dichiarava verde avendo
    controllato formattazione e lint contro codice che non esisteva più. Quel
    giorno era comunque sano solo perché gli stessi comandi erano stati lanciati
    a mano nel frattempo — che è fortuna, non un cancello.

    Corretto facendo portare al marker di un passo dipendente dal sorgente
    l'**impronta dell'albero** che attesta — l'insieme di (percorso, mtime,
    dimensione) su tutti i `.rs` di `src/`, `tests/` e `crates/` — e ignorandolo
    quando l'albero si è mosso. L'impronta non passa da git di proposito: un
    passo deve rigirare per una modifica **non committata**, che è lo stato in
    cui questa finestra lavora sempre. Red-checkato nelle tre direzioni:
    albero fermo → `SKIP` di tutti e sei; un `touch` su un file → `STALE` e
    riesecuzione; e le riesecuzioni passano — cioè la suite è verde **contro il
    sorgente di adesso**, che è precisamente ciò che i marker vecchi
    nascondevano.

    I passi `build_*` restano deliberatamente fuori dalla regola (cargo decide
    da sé se ricostruire, e la provenienza la porta il checksum del binario), e
    così i `netns_*`, già coperti dalla regola del binario.

19. **`scp` non è più `scp`: `$HOME` in una destinazione remota è una
    DIRECTORY di nome `$HOME`.** OpenSSH 9 ha reso SFTP il protocollo
    predefinito di `scp`, e SFTP non lancia una shell: il percorso remoto è
    letterale. Il percorso del binario del jump host era scritto `\$HOME/bore-jump`
    — che funziona ovunque arrivi a una shell remota (`ssh host "sha256sum
    \$HOME/..."` lo espande) e fallisce dove arriva a SFTP. Prima esecuzione di
    P6: `scp: dest open "$HOME/bore-jump": No such file or directory`, e tutte e
    tre le fasi morte in provisioning.

    Corretto con **una** grafia che vale in entrambi i posti, il che significa un
    percorso assoluto, il che significa chiedere alla macchina remota dove sia la
    sua home — una volta, e rumorosamente: una home non risolta produrrebbe in
    silenzio `/bore-jump`, che l'utente della VM non può scrivere, trasformando
    un difetto di coordinate in un errore di permessi tre fasi più in là.

20. **Una riga di provenienza che non trova il suo soggetto stampava il
    vuoto.** `CARGO_TARGET_DIR=target/jump` mette l'artefatto in
    `target/jump/**release**/bore`; il driver del jump host leggeva
    `target/jump/bore`, `sha256sum` falliva dentro un `2>/dev/null` e
    l'intestazione stampava `binary:` seguito da niente. Una campagna la cui
    ripetibilità si regge sull'annotare **quale binario** ha prodotto un numero
    non può permettersi che quell'annotazione fallisca in silenzio. Ora, se il
    file non c'è, lo dice.

21. **Una pipeline non è un controllo: `cmd | head -c N` esce 0 con zero byte.**
    `jump_wchan_ms` finiva in `... | head -c 1 >/dev/null || { echo FAILED; }`,
    sopra un commento che affermava che leggere un byte dimostrava che il canale
    trasportava dati. Non lo dimostrava. Con **nessun sshd** dietro il provider
    la funzione ha pubblicato **1270,3 ms** — il tempo che ci metteva a fallire
    — come latenza, mentre la funzione gemella sullo stesso percorso riportava
    onestamente `FAILED`. È la firma di questa campagna («uno zero che significa
    che lo strumento si è rotto») travestita da pipe, e **la guardia
    `INSTRUMENT FAILURE` aggiunta il giorno prima non poteva prenderla**:
    verifica che esista almeno un campione, e un campione c'era. Una guardia che
    conta i campioni non protegge da un campione inventato.

22. **La premessa di una misura ottenuta per sottrazione va verificata.** Su
    questa workstation **non c'è nessun sshd** (`openssh-server` non è
    installato, la 22 risponde `Connection refused`) e il provider del jump host
    è stato puntato su `127.0.0.1:22` per tutta la vita della campagna. I due
    risultati di P6 sono differenze: un termine assente non fa perdere una
    colonna, si **ridistribuisce** sull'altra. Ora `jump_require_inner_target`
    rifiuta di far misurare una fase senza estremo interno, e
    `jump_inner_target.sh` ne alza uno vero (OpenSSH in container, su loopback,
    rimosso dal driver a fine campagna).

23. **Una redirezione dentro `ssh "..."` avviene sull'ALTRO lato.**
    `ssh host "wc -c > '$FILE'"` espande il percorso in locale e **scrive in
    remoto**: il file era sotto la `~/.cache` di questa workstation, quindi
    contro un estremo interno che è un'altra macchina ogni ripetizione stampava
    `No such file or directory`. Si cattura lo **stdout** del lato remoto in
    locale — e così la fase non ha più bisogno di nulla di scrivibile là.

24. **Una sonda che interroga sé stessa non può fallire — il gemello opposto
    dello zero.** `vpn_quic_timers` misurava la vita del tunnel con
    `ping $A_PEER`, ma `$A_PEER` (10.77.0.2) è l'indirizzo overlay **di questa
    workstation** — il nome che il *listener* dà a noi. Un ping al proprio
    indirizzo di TUN è risposto dallo stack locale **senza che un pacchetto
    entri mai nel tunnel**, quindi riusciva con il tunnel vivo, morto o mai
    costruito: tutti e **15** i bracci hanno riportato
    `NEVER-SILENT(blackhole did not bite)` e ogni mediana ha letto `n/a`.
    Stesso indirizzo sbagliato in `vpn_carriers`, dove `tcp_mbps $A_PEER`
    faceva iperf3 **contro sé stessa**: ogni cella di ogni ripetizione `FAILED`.
    Tutte le altre fasi VPN usavano già `$B_PEER`. Corretto in entrambe.

    E va detto quale ipotesi questo ha **smentito**: la prima lettura di
    `NEVER-SILENT` era «il fallback senza cuciture di DEC-2 funziona così bene
    che nessun pacchetto si perde», una conclusione lusinghiera e sbagliata che
    stava per essere scritta. La distinzione fra le due resta nel codice — se il
    tunnel non tace, la fase ora **chiede al server** se il percorso è passato a
    relay, e solo allora `dead=0` è una misura invece che un guasto.

25. **Un valore non numerico che diventa `0,000` nella colonna dei rapporti.**
    La guardia di `vpn_carriers` rifiutava `0|0.0|""`, non `FAILED`: la stringa
    passava, e `awk 'BEGIN{printf "%.3f", g/b}'` con `g="FAILED"` produce
    **0.000**, che entrava nella mediana dei rapporti come campione. `med()`
    rifiutava il `FAILED` nella colonna dei Mbit/s e **non poteva** rifiutare lo
    zero accanto: il guasto era invisibile esattamente nella colonna che un
    lettore confronta. Ora la guardia rifiuta qualunque non-numero.

26. **`ws_path` leggeva il log, con un'espressione che non ha mai potuto
    corrispondere.** Cercava `falling back to relay`; il prodotto scrive
    `direct path lost; fell back to relay (link preserved)`. Dopo un fallback il
    percorso continuava quindi a leggersi `direct` **per sempre** — che è la
    premessa esatta del peccato capitale di questa campagna, «un numero diretto
    che era silenziosamente un numero di relay». Qui è costato la colonna
    `back to direct` di `vpn_quic_timers`, che riportava **~30 ms** contro una
    griglia di ritentativo di **30 s**: tre ordini di grandezza, e la nota della
    fase stessa diceva che quel numero è quantizzato dalla griglia. La colonna
    `dead`, misurata **con pacchetti** e non con una riga di log, era invece
    corretta — il disegno che ha pagato.

27. **Non si aspetta un marker di completamento dentro un file che una
    esecuzione precedente ha scritto.** Attendere una fase con
    `grep -q '^DONE' out/fase.out` ha trovato il `DONE` del giro **prima**,
    perché il driver tronca il file solo quando la fase parte davvero e prima
    spende ~20 s di baseline. I numeri riletti erano quelli vecchi — identici
    all'ultimo byte, che è l'unico motivo per cui ce ne si è accorti. Si aspetta
    il **processo**.

28. **Una sola precondizione morta fabbrica un'intera colonna di fallimenti.**
    `jump_stab` ha riportato PASS 6 / FAIL 10. I dieci erano **uno**: un
    ControlMaster che era (correttamente) finito col suo trasporto, dopo di che
    ogni verifica che ne riusava il socket falliva — compresa quella che porta
    la promessa vera, che ha quindi condannato il prodotto per un errore
    dell'harness. Due regole: una verifica la cui precondizione è il soggetto di
    un'altra deve **ristabilirla**, non ereditarla; e quando una fase riporta
    molti fallimenti insieme si cerca **una** precondizione condivisa prima di
    credere a uno qualunque — un prodotto non si rompe di solito in dieci punti
    contemporaneamente, uno strumento sì.

29. **Il gate di build LINTAVA un set di feature di cui non ha mai ESEGUITO i
    test.** `clippy_jump` controlla `--features vpn,ssh-gateway`, ma
    `test_default` (feature di default) e `test_vpn` (`--features vpn`) **non
    compilano affatto `src/sshgw.rs`**: è dietro `ssh-gateway`. L'intera suite
    dell'ingresso SSH — le unit in `sshgw.rs` più `tests/ssh_gateway_test.rs` e
    `tests/ssh_jump_test.rs` — stava quindi **fuori dal gate**, e una modifica
    lì poteva passare P5 senza che un solo test la toccasse. Trovato mentre si
    aggiungevano i cancelli di I-SSH12: erano verdi a mano, e il gate non li
    avrebbe mai fatti girare. Aggiunto `test_jump`; alla prima esecuzione ha
    impiegato **586 s** — non era una lacuna teorica, era una suite intera.

---

## 5. Tunable: cosa è stato deciso, e cosa è stato falsificato

Ogni riga qui è una domanda **chiusa con una misura**, non con un'intuizione.

| tunable | decisione | perché |
|---|---|---|
| coda del device TUN (`txqueuelen`) | **128** | su rame la scala è piatta; su radio muove la latenza di 6× — bore gira anche su radio |
| controllo di congestione diretto | **`bbr`** | `newreno` dà +3,3 % ma alza il **minimo** di RTT sotto carico (26,1 → 33,4 ms) |
| buffer datagram in uscita | **8 MiB** | la curva **si rovescia** a 64 MiB; 32 MiB compra +3 % per +12 ms |
| buffer socket UDP | forzato a 16 MiB | il kernel lo taglia silenziosamente a `net.core.*mem_max` |
| `--carriers` sul percorso diretto | **1** | misura del 12/09 a **flusso singolo**: 4 carrier consegnano meno (359 vs 388). Rimisurato il 13/09 con 4 e 8 flussi: **neutri** (0,948 vs 0,954) — il pinning per flusso fa sì che un carrier in più non possa aiutare un flusso solo, e a 95 % della linea non c'è margine. Le due misure non si contraddicono: rispondono a domande diverse — §45.5 |
| `--carriers` sul **relay VPN** | **1**, e ora si sa di quanto | a flusso singolo i carrier **dimezzano o peggio** (0,824 → 0,286 con c2, → 0,440 con c4), con insiemi di campioni **non sovrapposti**. Meccanismo: il relay distribuisce per **datagramma** (DEC-7), il flusso TCP interno legge il riordino come perdita — BW-F2 sul percorso dove non è stato corretto, perché il pinning richiederebbe di ridimensionare la finestra di replay (DEC-10) — §45.5 |
| `BORE_DIRECT_QUIC_IDLE_MS` / `_KEEPALIVE_MS` | **invariati** (3 s / 10 s), ma ora **misurati** | il tempo di buio quando il diretto muore è **13,1 s**, e la coppia lo controlla: 4,7 s al braccio veloce, 40,8 s al lento. Non si abbassa il default perché un `idle` corto dichiara morto un percorso che ha solo avuto una pausa, e su radio quella pausa è comune; un operatore che sa di avere un percorso stabile può comprarsi 8 secondi — §45.4 |
| `BORE_DIRECT_QUIC_INITIAL_RTT_MS` | **irrilevante** per il recupero | `rtt-low` e `rtt-high` leggono 13 101 e 13 100 ms contro i 13 104 di `shipped`: governa il primo Initial di una connessione nuova, non la scoperta che una viva è morta — §45.4 |
| `--carriers` sul **relay pubblico**, sotto concorrenza | **1 di default; 4 aiuta, di quanto non è stabilito** | è la seconda metà di una regola di cui esisteva solo la prima: su un percorso ozioso i carrier danneggiano leggermente (c4/c1 **0,941**, campagna vhost), a 4 connessioni concorrenti aiutano in **tutti e tre** i round appaiati — ma di 1,457, 1,463 e **1,085**. Con un'escursione di cella del 38 % a questo gradino (§42) la *direzione* è consistente e la *misura* no: va citato «aiuta, fra l'8 e il 46 %», mai «+46 %». Il disegno è appaiato per round, quindi la deriva si annulla; è la varianza residua a non farlo. — §36.2, da rileggere con §42 |
| MTU della TUN **sul relay** | **proposto, NON spedito** | la scala **sale**: 1350 → 8000 dà +33 % giù e +38 % su, e la frazione del nudo va da 0,62 a 0,82. Vale solo sul relay (byte TCP, è il TCP a segmentare); sul diretto 1350 è un vincolo, non prudenza (datagrammi QUIC) — §29.3 |

**Falsificati e da non ritentare senza nuove prove:** la serializzazione
dell'uplink, la profondità della coda TUN come tetto, la CPU, e la perdita QUIC
(`lost_pct` 0,00 in ogni arm). *Non esiste un tetto di filo a ~400 Mbit/s: il
residuo verso il nudo è un divario di **latenza**, non di capacità.*

**Chiuso in questa finestra, e da non riaprire:** il deficit residuo del
percorso diretto **sono le intestazioni**. Il solo incapsulamento predice il
95,2 % del nudo (il tunnel mette sul filo il 10,39 % in più di quello che
consegna, contro il 5,14 % del nudo — 54 B in più per frame, cioè QUIC + AEAD +
UDP/IP esterno) e il rapporto misurato è ~0,95. I due numeri coincidono: non è
una perdita da recuperare, è aritmetica (§29.1).

**E una promessa verificata sul percorso vero:** sotto carico in **upload** il
percorso diretto paga **0,09 ms** di latenza aggiuntiva (19,84 → 19,93) e il
relay ne paga **37,8** (20,47 → 58,26). È la contropressione dell'uplink
(`send_batch_wait`, BW-F3) che fa esattamente quello per cui è stata scritta
(§29.2).

Ogni decisione porta la stessa riserva: tutti gli arm sono girati su `bbr` e su
un percorso **senza perdita**. Si riaprono con un percorso **lossy** nella
matrice, non con un altro numero di throughput a percorso singolo.

---

## 6. Il conto AWS

Dettaglio in §25–26 delle evidenze. In breve:

- AWS fattura **solo l'uscita**. Gli arm di **upload sono gratis**: metà delle
  fasi VPN non costano nulla e non vanno accorciate.
- VM e server di staging stanno **nella stessa VPC** ma l'harness li indirizza
  per **IP pubblico**: nelle fasi a forma di relay ogni byte consegnato si paga
  **due volte**. Misurato: **48 %** del fatturato.
- Tagli applicati: ~62 GiB (≈ €5,5, il 40 % di P7) senza rinunciare a una misura
  che potesse cambiare una decisione.
- Speso **di più** dove serviva: `ws_asym` decideva con **un solo campione per
  direzione**; ora tre ripetizioni appaiate a ordine alternato, +€0,08.

---

## 7. Come rifare tutto fra un mese

| cosa | dove |
|---|---|
| driver dello sweep | `scripts/perf/staging/rerun_eth.sh` |
| driver dell'attribuzione | `scripts/perf/staging/rerun_eth_p7.sh` |
| approfondimento VPN | `scripts/perf/staging/rerun_vpn_deep.sh` |
| jump host | `scripts/perf/staging/rerun_jump.sh` — tre fasi: `jump_lat` (latenza), `jump_hol` (isolamento fra canali), `jump_stab` (stabilità e fallback). La mappa **domanda del piano → fase** è in testa al driver, così una domanda scoperta si vede |
| finestra di build | `scripts/perf/staging/p5_build_gate.sh` |
| costo AWS | `aws_cost.sh` (finestra) e `cost_watch.sh` (per fase) |
| scansione segreti | `secret_scan.sh` — i pattern arrivano da `~/.config/bore-perf/env.sh`, **mai** dal repository |
| lint dell'harness | `lint.sh` — **sei compilatori**: `bash -n`, shellcheck mirato, `unbound_scan.sh` (variabili lette e mai assegnate), `shadow_scan.sh` (array che oscura uno scalare di libreria), `order_scan.sh` (chiamata di livello superiore prima della definizione), e il cancello V-11 che rifiuta un `sort -n` senza `LC_ALL=C`. Oggi: **144 file puliti** |
| guardia di contesa | `driverlib.sh` — sorgente dei quattro driver **e** della finestra di build: un solo file identifica un processo da `/proc/<pid>/cmdline`, mai dal testo della riga di comando |
| segreti nei risultati | `secret_scan.sh --out` — **prima** di citare un `.out` in un documento |
| runbook e **26 trappole** | `scripts/perf/staging/README.md` |
| coordinate e credenziali | `~/.config/bore-perf/env.sh` (fuori dal repo, 600) |

Ogni fase scrive `out/eth/<fase>.out` e un marker `_done.<fase>`; un driver
rilanciato riprende da dove era. Un `timeout` **non** scrive il marker: una fase
troncata si ripete, non sparisce.

---

## 8. Cosa resta aperto

- **Attribuzione per tratta del relay** — richiede una modifica ai security
  group AWS più una porta fuori dall'intervallo DNAT del server (§25.6).
- **D6/D7** (forwarding `LAN_HOST`, isolamento degli spoke dell'hub) — servono
  un terzo host. **P6 ha dato a questo un prezzo, non più solo un'etichetta:**
  con due soli host il client del jump sta sulla stessa macchina del provider,
  quindi ogni round trip dell'handshake **interno** attraversa la WAN due volte.
  I ~400 ms di `open - wchan` sono ~6 round trip di quella forcina, e gli 87 ms
  di `chan` ne sono due. Il termine che il progetto controlla — 46 ms — è
  misurato correttamente lo stesso, ma ogni assoluto di sessione **interna** è
  gonfiato dalla topologia e va ridichiarato con un client altrove (§44.3).
- **`--carriers` sul relay VPN a 4 e 8 flussi.** A **un** flusso la risposta è
  netta e negativa (§45.5). A 4 e 8 gli intervalli si sovrappongono quasi per
  intero e una cella è perfino bimodale (0,916 contro due valori a 0,38): la
  domanda «i carrier recuperano quando i flussi sono tanti quanto i carrier?»
  resta **aperta**, e per chiuderla serve la stessa medicina di n≥4 — poche
  celle, molte ripetizioni. Nota che la dispersione del **relay** arriva a 3,2×
  sulla stessa cella contro l'1 % del **diretto**: è il relay a essere
  variabile, non la misura a essere sciatta.
- **Il rekey non è mai partito da solo in `jump_stab`.** Parte solo perché la
  fase impone `RekeyLimit 256K 20` al client; il russh lato server non lo
  inizia mai in pratica. Quindi la verifica «il rekey attraversa la sessione»
  prova il caso **client**, e il caso **server** resta non esercitato sul
  campo. Non è una lacuna di prodotto nota, è una lacuna di copertura.
- **`jump_stab` non può separare «il relay costa» da «è passato del tempo».**
  La sua baseline è sempre prima e il relay sempre dopo (§44.9). La correzione è
  piccola — campionare una seconda baseline dopo il ritorno al diretto — e non è
  stata fatta in questa finestra. Nel frattempo la risposta la dà `jump_lat`,
  che i bracci li interleava: relay ed diretto sono **identici**.
- **N-9**, la coda di concorrenza del relay vhost — nessun meccanismo da
  colpire; si chiude solo rimisurando dalla stessa regione.
- **Perché l'upload del relay ha una rampa e il download no.** Campioni grezzi
  182 → 474 → 551 Mbit/s in ripetizioni successive contro un download stretto a
  459–470. Una fase a una sola ripetizione avrebbe pubblicato **182**, cioè il
  24 % del nudo invece del 63 %. I contatori di allowance ENA sono identici
  prima e dopo ogni braccio, quindi il bucket dell'istanza è scagionato per
  questi bracci; restano il riscaldamento della finestra QUIC e il tempo che il
  relay impiega a raggiungere il regime di CPU di §28.2 (§28.3).
- ~~**Rumore di coordinate nei file di risultato**~~ — **CHIUSO in P8.**
  `secret_scan --out` contava 48 righe. Due classi, due correzioni diverse:
  la maggior parte erano l'avviso `Permanently added '<indirizzo>' ...` che ssh
  scrive su stderr, ora spento con `-o LogLevel=ERROR` in `SSH_OPTS` (gli errori
  continuano a stampare; solo l'avviso tace) — red-checkato con un
  `UserKnownHostsFile` usa-e-getta, che è l'unico modo di provarlo: con l'host
  già noto l'avviso non esce comunque e il test passerebbe da solo. Le altre
  erano **etichette**: `ws_rtt`, `vpn_lat` e `vpn_profile` stampavano
  l'indirizzo sondato come nome di colonna, e ora stampano il **ruolo**
  (`test-vm`, `server`, `gateway`) — che è anche ciò che serve a chi legge.
  Restano le righe di `ss` in `vpn_ctrl_leak`, che sono evidenza grezza del
  kernel: manometterle danneggerebbe la prova, quindi la regola resta «non si
  citano», applicata da `--out` prima di ogni citazione.
- **Perché la scala di concorrenza diventa INSTABILE a n≥4** — la domanda che
  ha sostituito quella di partenza. A n=1 e n=2 la stessa cella si ripete entro
  il 6 %; da n=4 in su l'escursione misurata è del **19–48 %**, mediana 38,6 %.
  Due fasi hanno già tolto di mezzo i candidati facili: `pub/origin_cpu.sh`
  esclude la CPU su origine (3–4 % di un core), VM (9–20 %) e client (10–17 %);
  `pub/ws_conns_procs.sh` esclude la topologia dei processi del client (n
  processi separati non fanno meglio di uno, entro il 13 % e sotto il rumore).
  Restano non separati: il percorso di accept pubblico del server sotto
  concorrenza, lo scheduling dell'istanza, e il micro-bursting dell'allowance
  ENA. La fase che chiude è piccola — la stessa cella ripetuta **15 volte** su
  un solo gradino, con i contatori di allowance letti come delta attorno a ogni
  ripetizione — e finché non gira **il ladder pubblico si cita fino a n=2**
  (§42).
- **Il costo della PRIMA connessione di un tunnel `--udp`** — **ATTRIBUITO,
  con un residuo.** `pub/ws_first_conn.sh` ha girato: il pool diretto vale già 1
  **prima** che passi un byte e `direct_fallbacks` resta 0, quindi non è né la
  composizione del pool né un fallback travestito — è il controllo di
  congestione che parte freddo. Il residuo è che là il costo misura **4,2 %** e
  in §35 misurava 18,6 %: l'unica differenza strutturale è che questa fase
  registra i tunnel in anticipo e fa passare ~40 s prima del primo byte, il che
  suggerisce che il costo sia funzione del **tempo dalla registrazione** e non
  del numero d'ordine del trasferimento. Ipotesi, non misura: la chiude la
  stessa fase con il ritardo come asse (0, 5, 20, 60 s). Fino ad allora si cita
  il caso peggiore, 18,6 % (§40).
- Le riserve «percorso lossy» di §5.
