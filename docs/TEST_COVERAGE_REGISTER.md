# Registro di copertura dei test — flag spediti e gate che li verificano

**Perché esiste.** Il progetto è grande e le regressioni costano. Questo
documento non elenca quello che i test fanno: elenca **quello che nessun test
faceva**, come è stato accertato, e cosa è stato aggiunto.

**Come è stato accertato (rigenerabile, non a memoria):**

```sh
# PASSO 1 — ogni flag dichiarato, non una lista scritta a mano:
#           estrai ogni `long` dalle struct clap di src/main.rs (79 oggi).
# PASSO 2 — cerca la stringa letterale in tests/ E in scripts/:
for f in $(cat /tmp/flags.txt); do
    n=$(grep -rl -- "--$f" tests/ scripts/ 2>/dev/null | wc -l)
    [ "$n" -eq 0 ] && echo "da verificare: --$f"
done
# PASSO 3 — per ogni riga stampata, LEGGI il modulo che implementa il flag.
```

**Il passo 3 non è facoltativo.** Un test che esercita il meccanismo senza mai
nominare il flag è invisibile ai primi due, e la prima versione di questo
documento ha pubblicato tre caselle "zero ovunque" che erano false proprio per
questo (§3-bis). Il grep ordina la coda di verifica; non la chiude. Vale anche
per gli strumenti statici: `tokensave_test_risk` riporta `has_test: false` per
`control_frame_summary`, che ha invece tre asserzioni — quelle che provano che
un frame di autenticazione non finisce in chiaro nei log — semplicemente perché
l'arco di chiamata passa per un metodo di trait nello stesso file.

---

## 1. La distinzione che ordina tutto: VALORE contro CABLAGGIO

Quasi ogni buco trovato ha la stessa forma. La funzione pura che *decide* un
valore è ben testata; quello che *applica* il valore non lo è.

| caso | valore | cablaggio |
|---|---|---|
| policy delle rotte | `filter_accepted`, 6 test unitari | nessun gate passava `--accept-routes` |
| `txqueuelen` della TUN | `tun_txqueuelen_resolution` | `vpn_txqueue.sh` usa `ip link set`, scavalcando bore |
| PMTU | `pmtu_decision`, `pmtu_shrink_now` | `pmtu_monitor(.., pin)` non testato |

È la stessa regola che il progetto applica altrove (P-12): *il log prova che il
processo ne ha parlato, il kernel prova che l'ha applicato.*

---

## 2. Buchi trovati e CHIUSI

| flag | copertura prima | aggiunto |
|---|---|---|
| `--no-route-manage` | **zero ovunque** (10 punti in `src/`, 0 test) | unit `peer_routes_are_installed_without_no_route_manage` + `no_route_manage_installs_no_peer_routes_at_all` (coppia, il primo è il red-check del secondo); e2e `T-RF6` |
| `--accept-routes` (lista selettiva) | 0 netns, 0 perf | e2e `T-RF4`, tre casi: esatto, supernet, non correlato |
| `--refuse-all-routes` | 0 netns | e2e `T-RF5`, incluso "l'overlay continua a funzionare" |
| `--no-udp-adaptive-plan` | **zero ovunque** — è il kill switch documentato | unit `the_adaptive_plan_kill_switch_actually_kills_the_plan`, con braccio di controllo a switch acceso |
| `BORE_VPN_TUN_TXQUEUELEN` (cablaggio) | solo risoluzione | stage `vpn_wiring.sh`: default 128, override, `0`, e i due clamp, letti dal kernel |

| `--pin-mtu` (cablaggio) | zero ovunque | e2e `T-PINMTU` in `vpn_netns_test.sh`: braccio di controllo senza il flag (la TUN **deve** muoversi quando la MTU del percorso scende a 1280), poi lo stesso stimolo con `--pin-mtu` (la TUN **non** deve muoversi). Se il braccio di controllo non si muove il gate dichiara `SKIP`, mai `PASS` |
| `--nat-udp-release-timeout` | zero ovunque | la decisione era scritta **due volte** (`client.rs` e `secret.rs`, inline nei bracci del `match`): estratta in `holepunch::preferred_port_verdict` e gated da `an_unreachable_stun_probe_moves_the_preferred_port_flag_nowhere` |
| lease di port mapping (cablaggio) | il valore era coperto, l'arm del `select!` no | unit `lease_changed_pends_unless_there_is_a_real_change`: un braccio dormiente che *ritorna* è un busy loop, non un no-op |
| **HOL fra canali SSH sul jump host** (il motivo per cui russh è vendorizzato) | **solo unit**, e questa è la classe che i test in processo falsificano: su loopback la coda non si forma mai | stage `jump/jump_hol.sh`: trasferimento bulk **limitato nel tempo** su un SECONDO canale della STESSA sessione, latenza di tasto campionata **dentro** il carico; il rapporto `loaded/idle` è l'isolamento. Limitato nel tempo e non nei byte, perché un conteggio di byte cade in punti diversi su bracci con throughput diverso e i campioni finirebbero fuori dal carico |
| **stabilità del jump host**: rekey attraversato, ritorno sul relay warm quando l'UDP muore, una riga di admin per alias | **nessuna fase** — il piano P6 la elencava come quinta domanda e non esisteva nulla che la ponesse | stage `jump/jump_stab.sh`: uccide il percorso diretto come lo uccide il campo (tabella nft che scarta l'UDP verso **un solo** indirizzo, quindi il relay TCP verso lo stesso host sopravvive) e verifica che la sessione esterna non muoia, che un canale NUOVO si apra comunque, che `direct_fallbacks` **si muova** (un'etichetta che cambia è prova più debole di un contatore che sale), che l'alias tenga una sola riga, e che il percorso torni diretto. PASS / FAIL / **SKIP**: una verifica non valutabile non è mai un PASS |

| **liveness della connessione mux (M-1)** | **zero ovunque** — il difetto era misurato sul campo (1 socket `ESTABLISHED` per riconnessione) e nessun test poteva vederlo | unit, gruppo "Connection liveness" in `src/mux.rs`, **quattro** test perché il primo da solo è soddisfatto da tre correzioni sbagliate: `a_connection_whose_handles_are_all_dropped_closes_itself` (red-check: senza il conteggio va in timeout, che è il sintomo di produzione), `a_connection_with_a_live_substream_keeps_driving` (rifiuta "esci quando cade l'Opener"), `a_connection_closes_when_its_last_substream_is_dropped` (rifiuta "esci quando cadono Opener E Acceptor"), `an_opener_alone_keeps_the_connection` (rifiuta qualunque correzione che guardi l'Acceptor: il pool del server tiene solo l'Opener). Gate di campo netns `T-CTRLLEAK` in `vpn_netns_test.sh` — stessa asserzione senza VM né WAN, quindi prende una REGRESSIONE e non solo la correzione |
| **coalescing delle scritture relay (V-14a)** | il formato era coperto, il NUMERO di scritture no | unit `relay_writer_coalesces_queued_frames_into_one_write` (8 frame già in coda ⇒ **una** scrittura, byte identici alla concatenazione, e il parser del peer li ritira uno per uno dal batch) + il suo gemello `relay_writer_sends_a_lone_frame_without_waiting_for_company` (clock in pausa: un writer che aspettasse un timer non potrebbe aver scritto) — senza il secondo, "una scrittura" è anche ciò che produce un writer che ATTENDE, aggiungendo fino a 31 pacchetti di ritardo |
| **AEAD relay a una allocazione (V-14b)** | il ROUND TRIP era coperto (`aead_roundtrip_ok`, `sealkey_matches_free_fns` su 25 byte), i BYTE SUL FILO a dimensioni reali no: un round trip passa anche se entrambe le estremità sbagliano nello stesso modo, e il peer in produzione non usa la nostra `seal` ma il suo `open` | unit `sealkey_frame_is_byte_identical_across_sizes` — le funzioni libere `crypto::seal_with_counter`/`crypto::open` restano di proposito nella forma vecchia e fanno da ORACOLO; confronto byte per byte a 0/1/1350/1500/65535 byte e tre contatori, più l'invariante del prefisso di lunghezza (`8 + len + TAG_LEN`): un errore di TAG_LEN lì desincronizza il framing del peer invece di far fallire l'AEAD, cioè si manifesta come link morto e non come errore crittografico. Gemello `sealkey_open_rejects_tampering_and_short_frames`: un `open` che tronca invece di copiare deve comunque rifiutare un bit di ciphertext, un bit di tag e un frame troppo corto |
| perdita sul percorso diretto (V-15) | nessuno stage stampava la perdita accanto al rate | strumento, non gate: `ws_quic_since` (lost/sent/cong + range RTT del carrier) e `ws_nic_drops` (tx_dropped/tx_errors di questo capo) in `vpnlib.sh`, cablati in `vpn_stability.sh`. Un braccio lento con `lost=0` e uno con `lost>0` sono diagnosi opposte e prima erano indistinguibili nell'output |
| `--udp-candidate` + `--udp-no-stun` (cablaggio) | 2 test cargo, nessun gate di campo | e2e `T-NAT-MANUAL-CAND` (il forward statico dichiarato dall'operatore porta la coppia a DIRECT senza alcuna discovery) + `T-NAT-NOSTUN-BARE`, il suo red-check: stesso router, stesso forward, senza la dichiarazione → RELAY |
| `--no-udp-adaptive-plan` (cablaggio) | il valore era appena stato coperto, il cablaggio no | e2e `T-NAT-PLAN-KILL`: il server non calcola più un piano e nessuno arriva al peer, **e la coppia resta DIRECT**. Il controllo è la corsa che è già avvenuta — ogni cella precedente gira contro un server senza il flag, quindi `server.log` deve contenere la riga la cui assenza si asserisce |

| `parse_transfer_quota` (il parser dietro **sette** flag) | **zero** | unit `transfer_quota_suffixes_are_si_or_binary_exactly_as_documented`: `k/m/g` decimali contro `ki/mi/gi` binari, maiuscole e spazi, e ogni valore illeggibile è un errore — mai un default silenzioso. Un misparse qui non fallisce: riconfigura in silenzio tutte le finestre QUIC |

| `BORE_DIRECT_QUIC_CC` / `BORE_DIRECT_QUIC_GSO` | i valori erano letti dall'ambiente **sul percorso dati** senza alcun test | unit `direct_congestion_unset_or_mistyped_is_the_shipped_controller` (V-12: `bbr` è una decisione, e un refuso non deve cambiare il controllo di congestione né far panic) + `direct_gso_is_on_unless_explicitly_turned_off` |
| `SprayTuning::enabled()` — l'interruttore della Fase 7 | **zero**, e tre celle netns ci si appoggiano | unit `the_sprayed_escape_is_off_when_any_of_its_three_budgets_is_zero`: ciascuno dei tre zeri asserito **da solo**, perché una catena `&&` che perde un termine passa comunque qualunque test che li azzeri tutti insieme |

| `--persistent` (transfer listener) | **zero ovunque** | e2e `transfer_persistent_listener_keeps_serving_after_a_failed_transfer`. L'asserzione che conta non è "il secondo invio è riuscito" ma che **lo stesso task** fosse ancora vivo fra i due (`!listener.is_finished()`): ogni altro test di resume in quel file avvia un SECONDO listener, quindi nessuno di essi può vedere questo flag funzionare |
| `--confirm-timeout` (transfer) | **zero ovunque**, e il ramo era irraggiungibile da un test | e2e `transfer_confirm_timeout_rejects_only_when_the_answer_outlasts_the_bound`, due bracci con una sola variabile (1 s contro 60 s, stessa risposta lenta di 6 s). Ha richiesto un seam nuovo, `BORE_TEST_CONFIRM_DELAY_MS`, accanto a quelli già presenti in `transfer::test_seam` e nella lista di `warn_if_active`: senza tty la lettura fallisce subito e con `BORE_TEST_CONFIRM_RESPONSE` risponde subito, quindi il timeout non poteva scattare in nessuno dei due casi |

| **il carico del gate HOL non era CONTATO** — il gate stesso poteva falsire | `jump_hol.sh` misurava `loaded/idle` senza alcuna prova che il carico esistesse: se il secondo canale non si apre, `loaded` campiona una sessione **inattiva**, il rapporto legge 1,00 e il gate riporta **isolamento perfetto**. La risposta sbagliata è identica alla risposta desiderata, che è il posto peggiore dove mettere un difetto | il generatore è limitato nel tempo da `timeout` e il capo remoto è `wc -c`, che stampa **solo** a EOF — quindi il conteggio dei byte che hanno davvero attraversato il secondo canale esiste per costruzione, senza segnali (MISURATO: GNU `dd` non stampa nulla quando `timeout` lo chiude con SIGTERM). Sotto `LOAD_MIN_BYTES` (8 MiB) i campioni `loaded` di quella ripetizione vengono **scartati**, non pubblicati; e se il campionamento dura più della finestra di carico la fase lo dice, perché la coda misurerebbe una sessione già scarica e tirerebbe la mediana verso "isolato" |
| **costo della PRIMA connessione di un tunnel** | nessuna fase leggeva l'admin API *attorno* al primo trasferimento; `ws_dl1` lo riproduce due volte (−18,6 % e −21,1 %) e non può attribuirlo | stage `pub/ws_first_conn.sh`: registra un tunnel relay e uno `--udp` insieme, li guida **alternativamente**, e stampa rate + `current_path` + `direct_stream_opens` + `direct_fallbacks` + `direct_pool` **prima e dopo ogni trasferimento**. I tre candidati si separano da soli: `path=relay` con `fallbacks +1` = non è mai stato diretto; `path=direct` con il pool già pieno prima di ogni traffico = controllo di congestione a freddo; un pool che si riempie *durante* il primo trasferimento = carrier dialati pigramente. Il tunnel relay è il **controllo**: "il primo trasferimento è lento" interessa solo se non è vero per tutti |
| **il TETTO del ladder pubblico non era attribuito a un HOST** | §36 pubblicava «il relay crolla a n=4» leggendo un solo numero per cella: origine, VM, client e server erano indistinguibili, e nessuna fase campionava nessuno dei quattro *attorno* ai trasferimenti | due stage in serie, perché nessuno dei due basta da solo. `pub/origin_cpu.sh` campiona i tick dell'origine da `/proc/<pid>/stat` sulla VM, la VM da `/proc/stat` e il client locale con `/usr/bin/time`, usando n=4 come **controllo** (lì i due trasporti differiscono provabilmente, quindi la CPU che spendono non lega) — e si **rifiuta di girare** se pid o `CLK_TCK` non sono leggibili, perché una colonna CPU a zero per mancato campionamento è identica a un host scarico. Esclusa la CPU, `pub/ws_conns_procs.sh` varia l'unica cosa rimasta lato client, la **topologia dei processi** (1 processo × n connessioni contro n processi × 1 connessione), a parità di origine, protocollo, tunnel, byte e ora; entrambe le topologie sono cronometrate con l'orologio del *driver* e non della shell, e per `many` sul processo **più lento** — cioè con un handicap a suo carico |

**Nota sui test a coppie.** `no_route_manage_installs_no_peer_routes_at_all` da
solo passerebbe anche se `apply` non installasse *mai* rotte. È il test gemello
che, installandole senza il flag, rende significativa l'assenza. Stessa ragione
per il braccio di controllo del kill switch: senza, l'asserzione passerebbe
perché gli input non producevano un piano, non perché il flag ha funzionato.

---

## 3. Buchi trovati e ANCORA APERTI

| flag | copertura | perché non ancora chiuso |
|---|---|---|
| `--pin-mtu` | zero ovunque | `pmtu_monitor` prende una `DirectConn` reale e costruisce `RealRunner` al suo interno: non testabile unitariamente senza rifattorizzarlo. Serve un gate netns che restringa la MTU del percorso e verifichi che la TUN **non** si muove, con braccio di controllo senza il flag |
| `UpnpBackend` (il *fallback* di `--upnp`) | il backend PCP è coperto per intero, quello UPnP per nulla | `probe`/`renew`/`release_op` parlano SSDP+SOAP con un IGD reale attraverso `igd_next`: non c'è un livello di frame da montare in un test come per PCP. Servirebbe un IGD finto; il percorso PCP, che è quello tentato per primo, è invece gated end-to-end contro un gateway finto |
| `--try-port-prediction`, `--udp-candidate` | **la voce precedente era sbagliata**, vedi sotto | — |
| isolamento fra spoke dell'hub | `T-HUB*` in netns | **non misurabile** sul percorso reale con entrambi gli spoke su un host: il kernel risponde all'indirizzo locale senza passare dall'hub (§14 delle evidenze). Serve un terzo host |
| `--advertise` / `--forward-accept` oltre il gateway | `T-FWD`, `T-NAT*` in netns | sul percorso reale `LAN_HOST` non è impostato, quindi il forwarding verso un host *dietro* il gateway non è mai stato misurato |

---

---

## 3-bis. Una voce di questo registro era sbagliata, e il metodo è il colpevole

La tabella diceva `--upnp` **"zero ovunque"**, `--try-port-prediction` *"1 test
cargo, copertura debole"* e `--udp-candidate` *"2 test cargo, nessun gate di
campo"*. Riletto il codice: sono affermazioni false.

- `src/portmap.rs` ha **quattro** test, fra cui un **gateway PCP finto su
  loopback** che percorre acquire → renew → riavvio del gateway (epoch che
  regredisce → ri-acquisizione con endpoint NUOVO) → delete a lifetime 0 alla
  drop, più la matrice di rifiuto dei frame manomessi e la tabella dell'epoch.
- `--udp-candidate` ha `gather_manual_candidates_first_no_stun_skips_chain`,
  che asserisce sia l'**ordine** (il candidato manuale è il primo) sia il
  **kind** sul filo (`RouterMapped`, l'invariante che vieta di aggiungere una
  variante nuova) sia che `--udp-no-stun` non tocchi la catena.
- `--try-port-prediction` ha la **coppia** on/off, che è esattamente la forma
  che il §4 qui sotto pretende.

**Il difetto sta nel comando di audit in testa a questo documento:** cerca la
stringa letterale del flag dentro i file di test. Un test che esercita il
*meccanismo* senza mai nominare il flag — ed è il caso di tutti e tre — è
invisibile a quel grep. Un audit di copertura che conta occorrenze di stringhe
misura come sono scritti i test, non cosa coprono.

Regola che ne segue, e che vale quanto quelle del §4: **una casella "zero
ovunque" non è un risultato finché non si è letto il modulo che implementa il
flag.** Il grep serve a ordinare la coda di verifica, non a chiuderla.

---

## 3-ter. L'audit rifatto su TUTTI i flag, non solo su quelli VPN

Il §3-bis dice perché il primo metodo era sbagliato. Questo è il secondo, esteso
a tutte e otto le sottocomandi, e va rieseguito così:

```sh
# 1. estrai ogni `long` dalle struct clap di src/main.rs (79 flag oggi)
# 2. per ciascuno, cerca la stringa letterale in tests/ E in scripts/
# 3. per ogni "zero", LEGGI il modulo: un test che esercita il meccanismo
#    senza nominare il flag è invisibile al passo 2 (§3-bis)
```

Dieci flag sono usciti a zero al passo 2. Il passo 3 — quello che il primo audit
non faceva — li ha risolti così:

| flag | esito del passo 3 |
|---|---|
| `--backend-tls-sni` | coperto in `src/` (`parse_params_backend_tls_*`, `backend_tls_*`) |
| `--ssh-advertise-address` / `--ssh-advertise-port` | coperto (`secret_provider_banner_consumer_command_advertise`) |
| `--stall-timeout` | coperto: `with_stall_*` (quattro test) più `read_exact_idle_aborts_on_true_stall` |
| `--stun-alt-port` | coperto dalla matrice STUN in `holepunch.rs` (attributi OTHER-ADDRESS) |
| `--persistent`, `--confirm-timeout` | esercitati dai test di `transfer` |
| `--test-bandwidth`, `--test-transfer-quota` | **il flag** non è gated, ma il suo parser lo è ora — vedi la riga `parse_transfer_quota` al §2 |
| `--verbose` | cosmetico, nessun comportamento da fissare |

**Il risultato utile non è la tabella: è il parser.** `--test-transfer-quota`
sembrava un flag diagnostico marginale e la funzione che lo legge,
`parse_transfer_quota`, è la stessa che interpreta le **cinque** dimensioni di
finestra/buffer QUIC e `--udp-memory-budget`. Un flag periferico condivideva il
parser con il cuore della configurazione del percorso diretto, e quel parser non
aveva un test. È il motivo per cui questo audit si fa per *funzione raggiunta*,
non per *flag dichiarato*.

---

## 3.1 Una regola in più, trovata facendo girare i gate: il PRESUPPOSTO di un test

`invalidate_address` (in `tests/e2e_test.rs`) asserisce che `Client::new` dia
errore per un nome inesistente. È fallito il 13/09 sulla workstation, e la causa
non era nel prodotto: il router dell'ISP risponde a **qualunque** NXDOMAIN con
un indirizzo proprio, e via search domain
`nonexistent.domain.for.demonstration` risolve a **127.0.0.1**. Il nome punta
quindi al loopback — e siccome quel test rinuncia di proposito a
`SERIAL_GUARD`, il server di un altro test può essere in ascolto lì: la
connessione riesce e l'asserzione fallisce per una ragione che non riguarda il
codice.

Il test poggiava su un **presupposto sulla rete**, non sul prodotto. Corretto
verificando il presupposto invece di assumerlo: dove vale (CI, qualunque
resolver onesto) i due casi girano esattamente come prima; dove non vale
vengono saltati **rumorosamente**, con il motivo stampato. Gli altri quattro
casi — «risolve ma non è un server bore» e «non è nemmeno un URI» — non
dipendono dal resolver e restano.

**Regola.** Un test che dipende da una proprietà dell'AMBIENTE (un nome che non
risolve, una porta libera, un percorso senza perdita, un orologio monotono) deve
**verificare quella proprietà** e saltare dichiarandolo quando non c'è. Le due
alternative sono entrambe peggiori: fallire dà un allarme su un difetto che non
esiste, e ammorbidire l'asserzione perché «a volte non vale» cancella il test
anche dove l'ambiente è buono.

## 3.2 Il presupposto di uno STRUMENTO, non solo quello di un test

La regola di §3.1 riguarda un test che poggia su una proprietà dell'ambiente.
P6 ne ha prodotto la versione più cara: uno **strumento** che poggia su una
proprietà dell'ambiente e la trova assente.

La campagna del jump host pubblica due grandezze, ed entrambe sono
**differenze**: `wchan - tcp` è il costo del gateway, `open - wchan` è
l'handshake dell'sshd interno. Su questa workstation `openssh-server` non è
installato e la porta 22 risponde `Connection refused`, quindi l'estremo interno
non esisteva. Una differenza a cui manca un termine non perde una colonna: la
**ridistribuisce** sull'altra, in silenzio.

Il che è precisamente quello che è successo, perché la funzione che avrebbe
dovuto accorgersene finiva in una pipeline:

```bash
... | head -c 1 >/dev/null || { echo FAILED; return; }
```

`head` esce 0 **senza aver letto niente**, quindi il fallimento è stato
pubblicato come **1270,3 ms** di latenza. E la guardia `INSTRUMENT FAILURE`
scritta il giorno prima non poteva prenderlo: verifica che esista *almeno un
campione*, e un campione c'era.

**Due regole.**

1. Se conta il **contenuto**, va letto in una variabile e confrontato — mai
   dedotto dal codice d'uscita di una pipeline. `jump_wchan_ms` ora pretende
   `SSH-` (RFC 4253 impone che la stringa di identificazione cominci così).
2. Uno strumento che misura **per differenza** deve verificare che ogni termine
   della differenza esista, **prima** di misurare
   (`jump_require_inner_target`). Una guardia che conta i campioni non protegge
   da un campione inventato.

Il gemello opposto, trovato lo stesso giorno in `vpn_quic_timers`: una sonda che
non può **fallire**. La vitalità del tunnel era verificata con un ping al
**proprio** indirizzo di TUN, risposto dallo stack locale senza che un pacchetto
entrasse mai nel tunnel — quindi «vivo» con il tunnel vivo, morto o mai
costruito. Una sonda va puntata sull'**altro** estremo, e la domanda da farsi
davanti a un controllo che passa sempre è la stessa che si fa davanti a uno zero:
*cosa lo farebbe fallire?*

---

## 4. Regola per chi aggiunge un flag

Un flag nuovo non è finito finché non esistono **entrambi**:

1. un test sulla funzione che ne **decide** l'effetto (puro, deterministico);
2. un gate che ne verifica il **cablaggio** leggendo lo stato dal kernel o
   dall'API del server — mai dal log del processo che dovrebbe averlo applicato.

E se il flag dice "non fare qualcosa", serve il gemello che prova che senza il
flag quel qualcosa succede: è l'unica cosa che distingue un flag funzionante da
una funzione che non fa niente in nessun caso.
