# Phase 3 — Stdin, exec e backup con sudo

> **Intent:** aggiungere stream monouso senza spool, processo produttore supervisionato e verifica effettiva dei metadati TAR sotto sudo.
> **Shippable alone?** sì: completa i tipi di sorgente della feature.
> **Preconditions:** fase 2 (`phase_03.md`) DONE; HTTP fallibile, scope task, CLI file/ZIP e harness TLS pronti.

## State contract (mandatory)

1. Leggere [STATE.md](STATE.md). Con §1 OPEN, terminare/annullare l'unità descritta in §6. Eseguire gate disponibili §3 e confrontare §1/§7/§11; correggere lo stato sulla base dei risultati reali.
2. Aprire ogni sottofase in §1 prima di editare: Type sub-phase, ID, OPEN, Intent, Assigned, Next action; §6 `claimed — nothing written yet`.
3. Chiudere dopo gate verdi: ledger §4, aggiornare §§5,7–11, §6 `none — tree consistent`, §1 prossimo ID/none e timestamp. WIP commits off, nessun commit/push.
4. Se interrotti lasciare OPEN e contenuto preciso del lavoro incompleto/test mancanti in §6.

## Contratto accettato dall'utente

```sh
sudo tar -cpf - myfolder | bore transfer link --stdin --filename backup.tar
sudo bore transfer link --filename backup.tar --exec -- tar -cpf - myfolder
```

Entrambi trasmettono esattamente stdout del produttore, senza ZIP, compressione o spool. Stdin è un'unica occasione, niente replay; produttore esterno può partire prima del GET e bloccarsi sulla pipe piena. Exec parte soltanto al primo GET consumante e conserva UID/GID/privilegi del processo bore. Nessuna shell implicita; `sudo` intorno a bore è sufficiente per far eseguire anche tar come root.

GNU tar crea normalmente gli header mode/owner anche senza `-p`; `-p` controlla ripristino dei permessi durante estrazione. Esempio di restore: `sudo tar --numeric-owner --same-owner -xpf backup.tar -C restore`. `--same-owner` riguarda ownership, distinta da `-p`. ACL/xattrs richiedono opzioni tar dedicate su creazione/estrazione; non promettere fotografia atomica o fedeltà di metadati che il comando tar non include. R8: https://www.gnu.org/software/tar/manual/tar.html.

**Prove preparatorie già fatte durante l'intervista:** launcher Rust std::process::Command sotto sudo, GNU tar1.35; file root-only leggibile dal child e negato al controllo unprivileged, SHA identico; 9 oggetti con uid12345/gid23456, mode0640/0755/06750/02750/01777, symlink/hardlink ripristinati; estrazione con umask0077 e -p conserva mode, controllo --no-same-permissions cambia0640→0600; errore tar propagato2. Queste prove stabiliscono semantica OS/tar, NON sostituiscono T-LINK-EXEC-SUDO attraverso il bore implementato.

## Stati monouso

| Stato | GET valido | HEAD valido | Transizione |
|-------|------------|-------------|-------------|
| Ready | riserva atomicamente e avvia produttore | 200 metadata, zero consumo | Ready→Streaming prima di spawn/read |
| Streaming | 409 Conflict | 200 metadata, zero consumo | solo il proprietario può completare/fallire |
| Consumed | 410 Gone | 410 Gone | terminale, nessun replay |
| Failed | 410 Gone | 410 Gone | terminale, rilanciare A |

Richiesta sbagliata (path/metodo/header/body) non consuma mai. Nessuna richiesta Range produce ripresa: normale GET completo se Ready, altrimenti409/410. Preview/link scanner che fa GET può consumare lo stream: documentarlo. Sessione mantiene link e stato terminale fino a Ctrl+C; eccezione stdin interrotto durante GET: uscita nonzero immediata della sessione per chiudere la pipe e sbloccare il produttore esterno. Exec fallito rimane Failed con child già terminato/reaped.

## Sub-phases

### 3.1 CLI e prenotazione atomica monouso

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review parser/stati/concorrenza; agent-2 implementa.
- **Files:** `src/main.rs:1104` TransferCommand::Link introdotto fase1; `src/transfer_link_cli.rs`; `src/transfer_link/source.rs`; `src/transfer_link/oneshot.rs` NEW; `src/transfer_link/http.rs`; `tests/transfer_link_test.rs`.
- **Change:**
  1. Aggiungere --stdin bool e --exec bool mutuamente esclusivi e in conflitto con PATHS. Aggiungere campo command `Vec<OsString>` dopo separatore `--` (Clap last=true); comando non vuoto richiesto se exec, vietato altrimenti. Validare prima di rete/file output, non affidarsi a clap configuration non provata.
  2. --filename obbligatorio in entrambi i modi; max-downloads assente risolve1, esplicitamente1 accettato, >1 rifiutato. File/ZIP conservano default8. --carriers non è numero downloader e resta ammesso.
  3. Nessuna shell: trattare executable/argv OS come unità esatte, inclusi spazi e Unix non-UTF8. Child `-cpf`, `--numeric-owner`, `-C` non possono essere reinterpretati come flag bore. Testare file il cui nome inizia con trattino tramite `./-nome` e documentare separatore per exec.
  4. Stati protetti con mutex breve o CAS; mai lock trattenuto durante read/network/spawn/wait. Restituire lease proprietaria della prenotazione; drop prima della fine dopo inizio consumo →Failed, non Ready. Prenotazione fallita/richiesta malformata non avvia child/thread.
  5. HEAD produce metadata senza Content-Length se size ignota, senza producer né hash; non aggiungere Content-Length:0 come dimensione dello stream. HTTP/1.0 stream sconosciuto rifiutato prima di prenotazione (505), così incompletezza è rilevabile col chunked HTTP/1.1 richiesto dai client normali.
  6. Pubblicare nessuna size preventiva per stdin/exec, nemmeno se un particolare stdin è file seekable: nessun seek/prescan/spool. MIME application/octet-stream, filename attachment. File/ZIP invariati.
  7. Su piattaforme non Unix --exec è errore anticipato esplicito, prima della registrazione; parser può mostrare flag con nota supporto Unix. Proteggere tutte le import Unix con cfg, compilazione Windows/macOS/Android preservata.
- **Unit tests:** `exec_argv_preserves_spaces_flags_and_non_utf8`; `stdin_exec_paths_conflict_before_connect`; `oneshot_default_concurrency_is_one`; `two_gets_have_exactly_one_owner`; `head_and_invalid_requests_never_claim`; `consumed_failed_are_never_rearmed`; `http10_does_not_consume_unknown_stream`.
- **e2e tests:** T-LINK-ONESHOT — due GET simultanei: un200 e un409, esattamente un avvio; dopo successo410; HEAD prima/durante non consuma; GET errato lascia Ready.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK/G-NOUDP verdi; nessun bug nei default conditional Clap; unità chiusa in STATE §§1/4/6/11.

### 3.2 Stdin senza spool e shutdown anche con pipe bloccata

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review di backpressure/shutdown; agent-2 implementa.
- **Files:** `src/transfer_link/stdin.rs` NEW; `src/transfer_link/source.rs`; `src/transfer_link_cli.rs`; `src/transfer_link/http.rs`; `tests/transfer_link_test.rs`; `scripts/transfer_link_e2e.sh`.
- **Change:**
  1. Avviare la lettura stdin soltanto dopo prenotazione del primo GET. Usare UN thread std dedicato per sessione, read bounded256KiB e canale bounded2; `blocking_send` sul thread è ammesso, mai sul runtime Tokio. Non usare tokio::io::stdin o spawn_blocking per read infinito: un worker Tokio bloccato può impedire la chiusura del runtime.
  2. Il thread std può essere bloccato in read mentre il writer esterno mantiene la pipe aperta: questo non è cancellabile tramite token. NON fare join infinito su quel thread. In shutdown/fallimento del GET stdin, chiudere ricevitore, terminare la sessione e far ritornare il vero processo CLI; l'uscita OS chiude stdin e termina il thread. Il runner stdin è quindi una funzionalità del processo CLI, non una API in-process che promette teardown completo senza exit.
  3. La parte libreria testabile usa AsyncRead/canale iniettato; il worker std di stdin reale resta nel bordo CLI. Non lasciare un thread bloccato nei test Rust in-process: testare questo caso con child bore reale e deadline. Registrare chiaramente nei log la differenza fra EOF e interruzione.
  4. Se il downloader smette di leggere, la coda blocca il thread e infine il produttore sulla pipe, senza crescere RAM. Se il downloader si disconnette, il body rileva errore, sessione stdin termina nonzero, fd0 chiuso dall'uscita; il produttore esterno potrà ricevere EPIPE/SIGPIPE. Non inviare segnali a PID esterni che bore non possiede.
  5. EOF pulito → Complete con conteggio/digest; log `stream_ended` e non `producer_succeeded`. Con --stdin non si conosce exit status del tar a monte. Non usare pipefail come falsa prova acquisita da bore; pipefail è una misura della shell esterna.
  6. Dopo EOF e download riuscito il thread è terminato, join possibile e link rimane Consumed fino a Ctrl+C; niente secondo thread/GET. Prima del primo GET Ctrl+C non ha thread stdin da attendere.
  7. Errore lettura, chiusura canale senza terminale o cancellation → body fallibile senza chunk finale; digest non presentato come completamento. Non usare EOF di una pipe interna come surrogato di Complete.
- **Unit tests:** `stdin_backpressure_bounds_read_ahead`; `stdin_eof_has_unknown_producer_status`; `stdin_read_error_prevents_complete`; `stdin_empty_stream_is_valid`; `stdin_after_complete_cannot_restart`.
- **e2e tests:** T-LINK-STDIN — pipe binaria inclusi zero/nonUTF8, byte/hash esatti, no spool e410 sul secondo GET; T-LINK-STDIN-CANCEL — writer vivo senza byte/EOF, dopo GET SIGINT e SIGTERM fanno uscire bore entro7s; altro caso curl interrotto →bore nonzero entro7s e pipe liberata; writer esterno ha cleanup posseduto dall'harness.
- **Done:** G-E2E/gate Rust verdi; test con pipe aperta prova uscita processo reale, non presunto join; unità chiusa in STATE §§1/4/6/11.

### 3.3 Exec: privilegio ereditato, stdout binario e gruppo processi

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review obbligatoria di lifecycle/errori; agent-2 implementa.
- **Files:** `Cargo.toml:85` Tokio runtime e `:131` nix Unix; `src/transfer_link/exec.rs` NEW; `src/transfer_link/source.rs`; `src/transfer_link/http.rs`; `src/transfer_link_cli.rs`; `tests/transfer_link_test.rs`.
- **Change:**
  1. Attivare feature process su Tokio runtime (oggi solo dev Tokio la include); aggiungere nix0.29 feature process/signal. API sicure, forbid unsafe resta. R4 https://docs.rs/tokio/1.52.3/tokio/process/struct.Command.html, R10 https://docs.rs/nix/0.29.0/nix/sys/signal/fn.killpg.html.
  2. `tokio::process::Command::new(executable).args(argv)`, stdin null, stdout piped, stderr piped, process_group(0), kill_on_drop(true). Non impostare uid/gid, non effettuare privilege drop, non usare sudo interno/pre_exec/shell. Il child eredita cwd/env/credenziali di bore. Percorsi relativi riferiti alla cwd dell'utente.
  3. Spawn soltanto dopo GET prenotato. Errore spawn →Failed e502 se header non inviati; nessun auto retry. Conservare Child/PGID come risorsa del producer supervisor, non dentro task che può sparire senza teardown.
  4. Leggere stdout con buffer bounded e SHA256 verso body; drain stderr simultaneo per evitare deadlock. Stderr a blocchi, non lines() senza limite; massimo64KiB di coda diagnostica, rate-limit log16KiB/s con conteggio byte omessi e riepilogo. Nessun output()/wait_with_output() che accumula tutto. Non loggare argv completo o environment: possono contenere segreti.
  5. stdout EOF NON conclude HTTP. Attendere exit status e completamento dei task di lettura: exit0 e nessun errore IO/cancel →Complete; nonzero/segnale →Failed e body Error. Corpo senza Content-Length; non emettere terminatore chunked prima di questa decisione. Un programma che scrive tutto e poi esce2 deve far fallire curl/wget; stesso requisito per zero byte+exit2.
  6. Se stdout si chiude ma child continua, tenere body aperto e loggare stato `waiting_for_producer_exit`; niente timeout totale arbitrario che tronchi backup validi. Ctrl+C/errore socket resta sempre reattivo. Stderr non deve ritardare cancellation anche se child lo scrive continuamente.
  7. Cancellation/disconnessione/control loss/SIGINT/SIGTERM: marcare Failed, interrompere output, SIGTERM al gruppo posseduto, grace5s, SIGKILL se necessario anche se nel frattempo il leader è uscito ma restano discendenti nel gruppo; wait/reap del figlio diretto e attendere/cancellare reader task. Non interrompere escalation soltanto perché child.wait è tornato. kill_on_drop è solo rete di sicurezza, non sostituisce wait. ESRCH dopo exit è innocuo; altri errori sono loggati. Non segnalare PID non posseduti né riutilizzati: memorizzare ownership e inviare segnali durante lifecycle del child/gruppo, non da monitor tardivi staccati. I nipoti non sono normalmente waitable da bore: l'oracolo richiede nessun discendente vivo e figlio diretto reaped; reaping degli orfani compete al loro reaper OS/container.
  8. Il comando deve essere un produttore foreground, non daemonizzarsi/setsid per sottrarsi al gruppo. Documentare che servizi/background staccati non sono un uso supportato. Normale tar e discendenti nel gruppo sono supervisionati. Prova un nipote che ignora TERM per verificare escalation/reap senza colpire harness.
  9. Su successo, child reaped e reader terminati prima di Consumed; su errore Failed con child già ripulito. Nessun reconnect o GET successivo deve rilanciare il comando. Su errore HTTP dopo producer exit0 conservare esito trasferimento Failed: exit0 non prova consegna.
- **Unit tests:** `exec_inherits_effective_credentials`; `exec_stderr_never_enters_payload`; `exec_nonzero_after_stdout_eof_is_error`; `exec_spawn_failure_is_terminal`; `exec_wait_is_required_before_complete`; `stderr_flood_does_not_block_stdout`; `cancel_kills_owned_group_and_reaps`; `body_disconnect_overrides_successful_exit`.
- **e2e tests:** T-LINK-EXEC — avvio differito dopo GET, argv letterali, hash esatto, empty success; T-LINK-EXEC-FAIL — exit2 dopo output completo e dopo0 byte, curl/wget nonzero e mai completed su A; T-LINK-EXEC-CANCEL — leader termina su TERM ma grandchild lo ignora: entro7s nessun discendente vivo, figlio diretto reaped, registry pulito. Harness assume responsabilità di reaper degli eventuali orfani, senza pretendere waitpid del nipote da bore.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK/G-NOUDP/G-E2E verdi; review race/ownership e prova false-success superate; unità chiusa in STATE §§1/4/6/11.

### 3.4 Gate sudo e roundtrip metadati TAR

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus definisce e approva gli oracoli; agent-2 implementa test riproducibili.
- **Files:** `scripts/transfer_link_privileged_test.sh` NEW; `scripts/transfer_link_e2e.sh`; `tests/transfer_link_test.rs`; fixture utility test NEW solo se necessaria nello stesso scripts/.
- **Change:**
  1. Script eseguibile con modalità `all`, richiede root ottenuto tramite percorso assoluto sudo -n; mai sudo bash (sudoers del progetto usa script autorizzati). Build del binario corrente va fatta prima come utente normale. Directory fixture mktemp posseduta e trap cleanup scoped, nessuna modifica dati reali/utenti di sistema.
  2. Fixture: file root-only0600 e controllo unprivileged che non può leggerlo; altri uid12345/gid23456 (numeri, nessun useradd), mode0640/0755/06750, directory02750/01777, hardlink e symlink. Impostare chown prima di chmod per non perdere bit speciali.
  3. Avviare vero server TLS/vhost, poi vero `sudo bore ... --filename backup.tar --exec -- tar -cpf - <fixture>`; lo script già root avvia bore root equivalente. Scaricare con curl via URL pubblico test e verificare SHA256 A/B, bytes esatti e producer_exit0. Non sostituire bore con launcher std della prova preliminare.
  4. Estrarre come root con `tar --numeric-owner --same-owner -xpf backup.tar -C restore` sotto umask0077. Confrontare lstat su tutti gli oggetti: uid/gid, mode incl special bits dove applicabili, contenuti, target symlink, uguaglianza inode per hardlink. Non seguire symlink nel confronto e non pretendere mode chmod dei symlink se OS non lo supporta.
  5. Negative controls: senza root lettura root-only fallisce; --no-same-permissions al restore cambia0640→0600; tar input inesistente termina nonzero e curl deve fallire anche se alcuni header TAR erano già emessi. Se il negativo non discrimina, il positivo non prova il requisito.
  6. Testare perdita connessione durante backup e Ctrl+C: gruppo tar fermato/reaped; mai log di backup verificato. Nessuna promessa ACL/xattrs nel test base; se documentati esempi estesi, aggiungere fixture/test condizionato al filesystem con esito non-run esplicito.
  7. Rendere disponibili casi stdin/exec nel basic non privilegiato; il caso root ha gate separato obbligatorio. Mancanza sudo/capability è blocker del gate, non skip silenzioso né richiesta di allentare assertion.
- **Unit tests:** nessuno nuovo se le fixture helper sono semplici; validare comunque negative controls nel test reale.
- **e2e tests:** T-LINK-EXEC-SUDO — roundtrip owner/mode/special bits/link e hash attraverso bore; T-LINK-EXEC-FAIL/T-LINK-EXEC-CANCEL anche con privilegi root; T-LINK-NOSPOOL durante backup.
- **Done:** G-ROOT e G-E2E verdi, report con GNU tar/versione filesystem e risultati confronti; gate Rust verdi; unità chiusa in STATE §§1/4/6/11.

### 3.5 Update README.md

- **Model:** agent-3:haiku
- **Assignment:** documentazione; agent-1 verifica esattamente sudo/tar e limiti monouso.
- **Files:** `README.md` sezione Transfer link da fase1, `:2049` Secure file transfer, `:69` artifact client Docker e `:1154` deployment dove pertinente.
- **Change:** documentare --stdin, --exec, --filename obbligatorio, max-downloads1, HEAD409/410, GET di scanner consuma, producer differito exec/backpressure stdin, necessità di rilanciare A dopo interruzione, chiusura sessione stdin su errore del GET. Inserire i due comandi sudo del contratto e restore --numeric-owner --same-owner -xpf, distinguendo -p/ownership e ACL/xattrs/snapshot. Dire che --exec è Unix, foreground senza password prompt/daemon; child eredita privilegi, cwd/env. SHA256 A confrontabile B, pipe EOF non indica exit0. Per immagine scratch client, tar non presente: usare pipe host con docker -i SENZA -t, --workdir/-w corretto, oppure immagine esplicitamente dotata di tar; non promettere exec tar nell'immagine attuale. Preservare lingua/struttura/tono, nessun dettaglio moduli/piano.
- **Unit tests:** nessuno, documentazione.
- **e2e tests:** esempi native e sudo realmente eseguiti da T-LINK-STDIN/EXEC-SUDO; esempio Docker sarà validato nella fase4 prima della release, senza dichiararlo già provato qui.
- **Done:** README risponde senza ambiguità alla domanda sudo/-p e spiega quando un backup è verificato; G-ROOT/G-E2E e gate fase verdi; unità chiusa in STATE §§1/4/6/11 e Docs fase3 aggiornata.

## Phase gates

- G-FMT: `cargo fmt --all -- --check`
- G-LINT: `cargo clippy --all-features --all-targets -- -D warnings`
- G-BUILD: `cargo build --locked --all-features`
- G-UNIT: `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ --skip t_web_soak`
- G-SERIAL: `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`
- G-LINK: `cargo test --all-features --test transfer_link_test -- --test-threads=1`
- G-NOUDP: `cargo test --no-default-features --test transfer_link_test -- --test-threads=1`
- G-E2E: `bash scripts/transfer_link_e2e.sh basic`
- G-ROOT: `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/transfer_link_privileged_test.sh all`
- Regression guard: file/ZIP ripetibili invariati; stdin transfer legacy e Web non modificati; -p concerne tar e non ZIP.
- README: tutti i nuovi flag/limiti e sudo/restore corretti, comandi reali, nessuna garanzia di byte ricevuti sul disco B.

## Phase done criterion

Monouso atomico e bounded; exec root tar preserva bytes/metadati al restore dimostrato; produttore fallito fa fallire curl/wget; cancellation chiude processo/gruppo/runtime anche con stdin bloccato. STATE §11 fase3 DONE, unità chiuse e README aggiornato.
