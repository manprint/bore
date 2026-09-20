# Phase 4 — Guasti, banda, Docker e accettazione finale

> **Intent:** dimostrare comportamento end-to-end, fallback, limiti risorse, prestazioni e comandi di distribuzione della feature completa.
> **Shippable alone?** sì, completa i gate di release senza cambiare le decisioni prodotto.
> **Preconditions:** fase 3 (`phase_04.md`) DONE; file/ZIP/stdin/exec e gate sudo implementati. Nessuna ottimizzazione speculativa prima di misurare.

## State contract (mandatory)

1. Leggere [STATE.md](STATE.md); risolvere qualsiasi OPEN tramite §6. Eseguire gate applicabili §3, confrontare §§1/7/11 e correggere dichiarazioni errate.
2. Aprire sottofase in §1 prima di editare: Type sub-phase, ID, Status OPEN, Intent, Assigned, Next action; §6 `claimed — nothing written yet`.
3. Dopo gate verdi appendere ledger §4, aggiornare §§5,7–11, §6 `none — tree consistent`, §1 prossimo ID/none e timestamp. WIP commits off: niente commit/push.
4. Se interrotti lasciare OPEN con file/resti/gate precisi in §6; un test non eseguibile non diventa PASS.

## Vincoli di accettazione

Non cambiare shared endpoint UDP, tune TCP, carrier protocol, heartbeat o fallback per far passare un benchmark Link. Nessun timeout globale sul backup, niente buffering dell'intero file, nessuna migrazione QUIC→TCP del body in corso. Debug deve descrivere il percorso reale, errore e byte parziali. Massima banda significa assenza di regressioni misurabili rispetto al vhost equivalente e diagnosi esplicita del collo di bottiglia, non una velocità universale garantita.

Tutti gli harness possiedono PID/netns/porte/temp e li puliscono su exit/error/INT/TERM. Eseguire netns serialmente. Non copiare `pkill -f target/release/bore` dai vecchi script; seguire il divieto in `scripts/vhost_udp_concurrency_repro.sh:74`. Nessun test deve toccare sorgenti utente o deployment reale.

## Sub-phases

### 4.1 Fault di trasporto, reconnect e nessun falso completamento

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus approva topologia/oracoli e ogni correzione al transport; agent-2 implementa.
- **Files:** `scripts/transfer_link_privileged_test.sh` da fase3, `scripts/transfer_link_e2e.sh`, `scripts/transfer_link_netns_test.sh`, `tests/transfer_link_test.rs`, `src/transfer_link_cli.rs`, `src/client.rs:1616` hook scoped; `.github/workflows/e2e_netns.yml:76`.
- **Change:**
  1. `scripts/transfer_link_netns_test.sh` usa una rete isolata A/server/B e client reale curl. TCP controllo e vhost HTTPS disponibili, UDP sullo shared vhost endpoint. Usare namespace/regole posseduti, configurazione/rimozione bounded; sysctl host globali non modificarli silenziosamente. Loggare buffer UDP effettivi leggibili con ss.
  2. T-LINK-QUIC: UDP consentito, attendere pool ready e scaricare payload; osservare path DirectQuic sul mittente per il download e contatori/UDP traffico sul server. `--udp` o registrazione alone non provano uso direct. SHA esatto su B.
  3. T-LINK-FALLBACK: bloccare UDP prima del connect, scaricare con default e osservare RelayTcp automatico; poi --relay-only con UDP disponibile resta TCP. Tutti i carrier sono TLS, nessun payload plain sul controllo/relay. Prova N=1 e N=2, con server clamp rispettato.
  4. T-LINK-DROP: iniziare download grande su QUIC, attendere prova di bytes ricevuti e path reale, bloccare UDP. Download corrente deve fallire entro45s (include idle timeout transport), mai essere dichiarato completed o continuare magicamente su TCP. Aspettare rilevamento carrier morto; nuova curl sullo stesso link file riesce via relay con hash esatto. Non asserire fallback immediato mentre QUIC è ancora ritenuto vivo.
  5. Stesso guasto con stdin/exec: niente replay o secondo avvio del produttore; stdin esce nonzero e richiede rilancio, exec resta Failed/410 con child ripulito. Al client GET successivo non può produrre un TAR dall'offset residuo.
  6. T-LINK-RECONNECT: interrompere controllo/server dopo URL, riavviare server alle stesse porte/config. File sessione registra di nuovo lo stesso label/URL dopo cleanup del vecchio scope; download vecchio fallisce, nuovo riesce. Auth/cert cambiati in errore permanente terminano, senza loop silenzioso. Verificare log di retry e cause.
  7. T-LINK-CLEANUP esteso: 100 cicli registrazione→download→cancel, metà in QUIC e metà relay; registry non cresce, scope task zero, FD tornano al baseline con tolleranza documentata2 descriptor dell'harness. Vecchio monitor non rimuove carrier della nuova sessione, mapping socket/path non sopravvive lease.
  8. Test negativo reale: mantenere l'asserzione DirectQuic e bloccare UDP nel caso direct deve rendere il test FAIL, non farlo passare via relay. Disabilitare temporaneamente quell'asserzione nello stesso setup errato dimostra invece il falso PASS che l'oracolo evita. Ripristinare subito entrambe le mutazioni; nessuna modifica sperimentale rimane nel tree.
- **Unit tests:** `reconnect_does_not_rearm_oneshot`; `cancelled_scope_cannot_publish_path_for_next_scope`; `transport_failure_wins_over_source_success`; aggiungere soltanto regressioni delle correzioni concrete emerse.
- **e2e tests:** T-LINK-QUIC/T-LINK-FALLBACK/T-LINK-DROP/T-LINK-RECONNECT/T-LINK-CLEANUP/T-LINK-EXEC-CANCEL, con output client e kernel/server come oracoli, non soltanto log A.
- **Done:** G-ROOT, G-E2E e G-LINK-NETNS verdi, nessuna migrazione body o regressione transport esistente; gate Rust verdi; unità chiusa in STATE §§1/4/6/11.

### 4.2 Prestazioni, memoria, slow consumer e logging

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review risultati prima di autorizzare ottimizzazioni; agent-2 misura/corregge.
- **Files:** `scripts/transfer_link_privileged_test.sh`, `scripts/transfer_link_e2e.sh`, `src/transfer_link/stats.rs`, `src/transfer_link/http.rs`, `src/transfer_link/source.rs`; eventuale utility benchmark NEW in scripts/, nessun nuovo binario di produzione.
- **Change:**
  1. Fixture throughput: file grande già in page cache o fonte deterministica veloce, sink /dev/null o hash streaming; separare costo disco/produttore da rete. Costruire release corrente prima del test. Baseline: stesso bore vhost, stesso HTTP origin bounded e stessa sorgente, stesse TLS/carriers/MTU, stessa macchina/rete; differenza soltanto orchestrazione Link. Annotare CPU/kernel/config e hash attivo in entrambi i lati.
  2. Misurare almeno3 run dopo warmup, mediana goodput payload, TTFB dal GET (esclusa digitazione/attesa umana), CPU user/system A/server, RSS massimo, ritrasmissioni/scarti UDP e byte kernel. Separare raw, ZIP STORED, stdin/exec; separare direct QUIC e relay. Non sommare overhead di TCP/TLS al payload scaricato. Il gate locale registra direttamente velocità e delta RSS; metriche non disponibili sul runner sono dichiarate come non osservate.
  3. T-LINK-PERF: raw Link mediana ≥90% baseline vhost equivalente in direct QUIC; per relay il gate accetta ≥75% e registra esplicitamente il costo della SHA-256 obbligatoria e della pipeline source HTTP rispetto al vhost kernel-splice. La soglia relay è una deviazione misurata e documentata, non un disabilitatore di integrità. TTFB aggiuntivo mediano ≤100ms su loopback/LAN fixture calda. Concorrenza1/3/8 e carriers1/2: nessuna serializzazione accidentale dei download; più carrier possono aiutare connessioni distinte, non dividono una singola curl in stripes. Nessuna promessa speedup N×.
  4. Se fallisce: identificare CPU hash/ZIP, syscall, lock/log/copie, buffer rete effettivi. Ottimizzare soltanto punto dimostrato; benchmark prima/dopo a parità di setup. Non disabilitare SHA/TLS, non aumentare buffer illimitatamente, non toccare SO_RCVBUF/SO_SNDBUF TCP (autotuning esistente), non aggiungere endpoint QUIC paralleli. Registrare deviazione motivata e review se cambia il piano.
  5. T-LINK-MEMORY: inviare almeno15GiB logici via stdin con pipe senza spool, sink veloce hashato e prova di backpressure/cancellazione con sink lento nei gate fault/e2e. RSS dopo warmup indipendente dai byte totali; manifest piccolo/1 download: delta A≤128MiB, server≤64MiB. Il processo mittente non deve creare payload né richiedere15GiB liberi. Producer sintetico non alloca15GiB. Nessun filesystem enorme necessario; output B verso /dev/null o hash.
  6. Prova destinatario che smette di leggere senza chiudere: producer si ferma sulla backpressure, memoria resta bounded; Ctrl+C termina entro7s. File/ZIP con lettore lento non bloccano altri download oltre banda/disco condivisi. HEAD/errori restano reattivi nel limite connessioni configurato.
  7. T-LINK-NOSPOOL: monitorare directory/fd del processo server e A durante15GiB, non soltanto a fine test. Consentire certificati/log test; vietare file payload anche cancellati ma ancora aperti. Evitare dump di payload nei log o packet capture del plaintext.
  8. T-LINK-OBSERVABILITY: controllare stdout esattamente una URL; progress stderr al ritmo scelto; download_id/path/bytes/failure/success coerenti; failed non ha success/digest completo; debug contiene causa e fase senza secret/argv/token URL. Rete/logging non possono attendersi reciprocamente: flood stderr child resta bounded/rate-limited.
- **Unit tests:** `stats_terminal_outcome_is_written_once`; `progress_tick_does_not_block_transfer`; `byte_totals_are_u64`; test regressione di lock/buffer soltanto se problema misurato.
- **e2e tests:** T-LINK-PERF/T-LINK-MEMORY/T-LINK-NOSPOOL/T-LINK-OBSERVABILITY, risultati numerici in STATE §7 (o file evidenza referenziato, non secondo status board).
- **Done:** G-ROOT/G-LARGE/G-E2E/G-LINK-PERF verdi, benchmark con setup e misure ripetibili, soglie rispettate senza allentare integrità; unità chiusa in STATE §§1/4/6/11.

### 4.3 Docker reale, CI e regressione completa

- **Model:** agent-2:sonnet
- **Assignment:** implementazione harness/CI; agent-1 review accettazione e compatibilità prima della chiusura.
- **Files:** `scripts/transfer_link_container_test.sh` NEW, `docker/Dockerfile.client:32`, `Dockerfile:42`, `.github/workflows/ci.yml:44`, `:62`, `:305`, `.github/workflows/e2e_netns.yml:76`, script Link e `tests/transfer_link_test.rs`.
- **Change:**
  1. Costruire immagine cliente dalla ricetta REALE `docker/Dockerfile.client` (scratch, entrypoint /bore), non `Dockerfile.client` in root che è launcher Alpine legacy. Identificare build args/artifact della pipeline e usare revisione corrente; mai pull di client-1.2.0-rc.2 come se includesse la feature futura.
  2. Test raw/misto con bind mount readonly e `--workdir /dir` o `-w /dir`, network host su Linux. Nessun --privileged richiesto per servire file normalmente leggibili. Permessi root richiesti solo se sorgenti lo richiedono; --privileged non è sostituto di spiegare UID/mount.
  3. T-LINK-DOCKER stdin: producer host `sudo tar ... | docker run --rm -i ... transfer link --stdin --filename backup.tar`; niente `-t`, perché TTY non è pipe binaria trasparente. Verificare zero bytes modificati, metadati TAR al restore e segnali Docker stop. Test di stdin con caratteri0x00/0x0a/0x0d/0xff per discriminare TTY.
  4. Stock scratch non contiene tar/shell/sudo: --exec tar lì deve fallire chiaramente prima del body riuscito, senza hang. Documentare exec nativo già testato; immagine con tar eventuale esplicita, non aggiungerla silenziosamente al client base. Non affermare preservazione owner host con rootless/usernamespace diverso senza un test dedicato.
  5. CI: agganciare G-LINK a test Rust ordinari, G-E2E Linux con curl/wget/openssl/python/unzip; G-ROOT netns seriale, G-LARGE job distinto se costo elevato, G-DOCKER su runner Docker. Strumenti/capability mancanti fanno fallire job richiesto o marcano non-run bloccante, mai PASS per skip automatico.
  6. Eseguire G-FULL una volta come regressione finale, oltre ai gate CI già suddivisi; includere soak Web escluso dal rapido se eseguibile secondo harness corrente. Eventuali test ignored/precondizioni annotate, nessuna dichiarazione che siano passati senza eseguirli. Non modificare test legacy per adattarli a regressioni introdotte.
  7. Eseguire gate netns vhost standard/hard/UDP concurrency già esistenti e SSH gateway privilegiato quando scope Client li tocca; comandi esatti dal workflow corrente devono essere riportati in STATE §7. Nessun test netns parallelo. Se un gate baseline era già rotto, isolare prova prima/dopo e mantenere blocker esplicito, non dichiarare zero regressioni per assunzione.
  8. Build matrice --no-default-features e target OS della CI, con exec cfg Unix; features process/signal nuove non devono rompere Windows. Nessun test che richiede GNU tar/root viene eseguito senza guardia di piattaforma in job Windows.
  9. Review finale requisito→T-ID→risultato: tutti i T-ID in STATE §11 hanno prova reale. Fix emersi hanno test discriminante e ledger; ritestare soltanto superficie rischiosa e gate richiesti dopo il fix, non lasciare optional work aperto.
- **Unit tests:** `docker_usage_matches_parser` solo se esiste helper CLI appropriato; non creare snapshot che replica l'implementazione. Le prove principali sono processi/container veri.
- **e2e tests:** T-LINK-DOCKER/T-LINK-COMPAT e tutti gli acceptance ID; root/netns/regression secondo workflow reale.
- **Done:** G-FULL/G-DOCKER e tutti i gate applicabili verdi; CI allineata, zero regressioni dimostrate; unità chiusa in STATE §§1/4/6/11.

### 4.4 Update README.md

- **Model:** agent-3:haiku
- **Assignment:** documentazione completa; agent-1:opus lettura finale obbligatoria prima di considerare la feature pronta.
- **Files:** `README.md:69` artifacts, `:938` server flags, `:1083` admin, `:1154` Docker Compose, `:2049` Secure file transfer/sezione Transfer link, `:3165` Vhost, `:3255` flags, `:3475` access logging, `:3537` E2E recipes.
- **Change:**
  1. Consolidare manuale utente da install/deploy fino al download: versione che include Link, wildcard DNS/cert già presenti, server HTTPS e TLS controllo, UDP endpoint/firewall e fallback, native/Docker/compatibilità server con --ssh-gateway. Non inventare modalità `ssh transfer link` o endpoint server aggiuntivo.
  2. Tabella TUTTI i flag/default/env reali: paths, --to/BORE_SERVER, --secret/BORE_SECRET, --ca-cert, --relay-only, --carriers, --filename, --stdin, --exec/separatore, --max-downloads condizionale, --stats-interval, -v/-vv/RUST_LOG. Assenza --insecure intenzionale. Esempi corrispondenti all'help.
  3. Comandi collaudati file, misto ZIP, curl/wget, backup sudo exec, pipe stdin, Docker corretto con -i/noTTY, restore TAR e sha256sum confronto; spiegare UID/mount e scratch senza tar. Nessun esempio Docker --wd inesistente, nessun vecchio tag presentato come contenente Link.
  4. Troubleshooting osservabile: TLS/CA/HTTPS assente, UDP bloccato, QUIC perso e nuova curl, stdin da rilanciare anche su A,409/410/503, file modificato, produttore exit!=0, stderr flood, cap/download/manifest, TTFB e banda. Descrivere successo invio vs salvataggio B; EOF stdin vs exit exec; niente E2E dopo terminazione TLS sul server.
  5. Conservare struttura, lingua e tono del README; editare senza riscrittura generale. Solo informazioni per usare/configurare il prodotto; nessun nome modulo/funzione, algoritmo interno, percorso piano o stato delle fasi.
- **Unit tests:** nessuno, documentazione; controllare link/flag solo con tooling già esistente se disponibile.
- **e2e tests:** eseguire ogni classe di esempio README con T-LINK-RAW/ZIP/STDIN/EXEC-SUDO/DOCKER; verificare output dichiarato. Non pubblicare esempi non provati come garantiti.
- **Done:** nuovo utente può installare, distribuire, usare e diagnosticare TUTTI i modi dal solo README; agent-1 approva, gate completi verdi; unità chiusa in STATE §§1/4/6/11, tutte le righe Docs aggiornate.

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
- G-LARGE: `bash scripts/transfer_link_e2e.sh large`
- G-DOCKER: `bash scripts/transfer_link_container_test.sh`
- G-LINK-NETNS: `sudo -n ./scripts/transfer_link_netns_test.sh` (seriale, root; direct/fallback/drop/reconnect/exec-cancel/100 cleanup)
- G-LINK-PERF: `BORE=target/release/bore BORE_PROXY_BUFFER_SIZE=16M bash scripts/transfer_link_perf.sh` (release; direct ≥90%, relay ≥75%, TTFB ≤100 ms, 15 GiB stdin/RSS/no-spool)
- G-FULL: `cargo test --all-features -- --test-threads=1`
- G-VHOST: `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test.sh`
- G-VHOST-HARD: `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test_hard.sh`
- G-VHOST-UDP: `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_udp_concurrency_repro.sh`
- G-SSH-ROOT: `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/ssh_gateway_test.sh`
- Regression guard: vhost standard/hard/UDP concurrency e SSH privilegiato esistenti quando applicabili, seriali; risultati in STATE §7.
- README: completo, esempi reali, nessuna affermazione E2E o conferma disco B, nessun dettaglio implementativo.

## Phase done criterion

Tutti i casi concordati sono implementati e provati: download standard ripetibili/paralleli, ZIP64, stdin15GiB bounded senza spool, tar sudo/restore, producer failure, QUIC/default/fallback/drop, shutdown/reconnect, Docker e logging. Prestazioni misurate contro baseline; la soglia relay ridotta è la deviazione documentata per il costo SHA-256/source pipeline, mentre direct mantiene il 90%; regressioni assenti, README sufficiente. STATE §11 fase4 DONE e nessun blocker/gate obbligatorio non-run. Non effettuare deploy/push: questa fase chiude implementazione verificabile, non autorizzazione alla pubblicazione.
