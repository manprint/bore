# Phase 2 — Client reali, banda, browser, documentazione

Intent: lo scenario di riferimento funziona con curl, wget e Chromium reali su un
`bore server` avviato da riga di comando con le sole variabili d'ambiente; la banda è
misurata contro la baseline vhost sullo stesso host; il server non scrive nulla su disco;
documentazione completa. CI verde.
Prerequisites: P1 DONE.
Phase closure: P2 (finale); review by agent-1:opus.

## State and ownership contract
Read STATE.md §0–1 first for scope, ownership, recovery, checks, and commit rules.
Open before edits; checkpoint at a recovery boundary; close with evidence. A
delegated worker follows its assigned OPEN unit. Missing design goes to agent-1:opus.

## Local design context
Plan revision: 1.

- **D1–D5, D9–D14** come in phase_01.md; gli script usano SOLO l'interfaccia pubblica: env `BORE_FAST_LINK_TRANSFER_*`, `curl`, `wget`, `openssl`.
- **D8/I-11** La banda si misura, non si stima: T-FL-PERF confronta mediane interleaved di 3 run per braccio, 1 GiB da tmpfs (`/dev/shm` se presente, altrimenti `$TMPDIR`), stessa macchina, stesso `BORE_PROXY_BUFFER_SIZE` per entrambi i bracci. Soglia: `fast ≥ 1.0 × vhost` (il fast link ha un salto e un mux in meno del relay vhost; se misura meno è un difetto). Se la soglia non regge su CI per rumore, NON abbassarla alla cieca: raccogliere i campioni grezzi (stampati sempre, V-11) e chiedere ad agent-1:opus.
- **I-2** transito: durante il braccio fast, `write_bytes` di `/proc/<pid>/io` del server aumenta ≤ 256 KiB e il picco `VmRSS` campionato a 10 Hz cresce ≤ 48 MiB rispetto al pre-trasferimento. stdout/stderr del server vanno in una pipe verso un processo `cat` (così i log non sono attribuiti al server).
- **I-4** codici di uscita: curl uploader 0 solo a download completato; 18 su scadenza/fallimento. curl downloader ≠ 0 su troncamento.
- **R1** stdout di curl in pipe richiede `-N` per vedere il link subito: gli script usano sempre `-N` e leggono la prima riga da un FIFO.
- **D22** browser: solo Chromium via Playwright con `--host-resolver-rules="MAP fast.bore.local 127.0.0.1"` e `ignoreHTTPSErrors: true`; Firefox/WebKit `test.skip` con motivo esplicito.
- Setup comune degli script (copiare da `scripts/transfer_link_perf.sh`): `set -Eeuo pipefail`, check prerequisiti, build release se il binario è più vecchio di `src/`, `mktemp -d`, `trap cleanup` che uccide ogni PID, CA + leaf openssl con SAN `DNS:bore.local,DNS:*.bore.local`, `free_port` Python, `wait_tcp`. Server:
  ```sh
  BORE_FAST_LINK_TRANSFER_ENABLED=true BORE_FAST_LINK_TRANSFER_VHOST=fast.bore.local \
  BORE_FAST_LINK_TRANSFER_AUTH="u:$PASS" "$BORE" server --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --control-port "$CP" --vhost-base-domain bore.local --vhost-mode both \
    --vhost-http-port "$HP" --vhost-https-port "$SP" \
    --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" 2>&1 | cat >"$tmp/server.log" &
  ```
  curl: `--cacert "$tmp/ca.pem" --resolve "fast.bore.local:$SP:127.0.0.1"`; wget: `--ca-certificate="$tmp/ca.pem"` e, poiché wget non ha `--resolve`, `https://127.0.0.1:$SP/...` con `--header="Host: fast.bore.local:$SP"` e `--no-check-certificate` (il certificato non copre l'IP; documentarlo nel commento). Ogni caso stampa `T-FL-E<n> PASS|FAIL <motivo>`; lo script esce ≠ 0 al primo FAIL, dopo aver stampato le ultime 40 righe di `server.log`.

## Sub-phases

### 2.1 `scripts/fast_link_e2e.sh` — curl/wget reali + CI
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus a P2.
- **Files:** READ `scripts/transfer_link_perf.sh`, `scripts/transfer_link_e2e.sh`, `.github/workflows/ci.yml` job `transfer-link`. NEW `scripts/fast_link_e2e.sh` (eseguibile, `#!/usr/bin/env bash`). WRITE `.github/workflows/ci.yml` (nuovo job `fast-link`, sotto).
- **Change:**
  Preconditions: P1 DONE.
  Casi (ognuno con payload proprio, SHA-256 confrontati con `sha256sum`):
  - E1 file CL: `dd if=/dev/urandom of=f.bin bs=1M count=64`; uploader `curl -sS -N -u u:$PASS -T f.bin https://fast.bore.local:$SP >up.out & ` (stdout su FIFO letto dal test per la prima riga); link regex `^https://fast\.bore\.local:[0-9]+/[a-z0-9]{16}/f\.bin$`; download `curl -fsS -o got.bin "$link"`; SHA uguale; uploader exit 0; ultima riga `# done:`.
  - E2 tar streaming: directory con 200 file casuali (incluso un nome con spazio e un file vuoto); `tar -cf - -C dir . | curl -sS -N -u u:$PASS -T - https://fast.bore.local:$SP/dir.tar`; link finisce con `/dir.tar`; download con wget (`-O got.tar`); `tar -tf got.tar | sort` == `tar -tf` di un tar di riferimento; estrazione e `diff -r`; entrambi exit 0.
  - E3 nome dal path: E1 scaricato con `curl -fsS -OJ` in una directory vuota produce `f.bin`.
  - E4 auth: senza `-u` → `curl --fail` exit 22 e codice HTTP 401 (`-w '%{http_code}'`); password errata → 401; nessuno slot creato (metrica via admin non necessaria: il GET del link non esiste).
  - E5 anteprime: con un upload in attesa, `curl -A 'Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)' "$link"` → 200 `text/html`; `curl -r 0-99 "$link"` → 416; `curl -I "$link"` → 200 con `content-length` e `content-disposition`; poi download normale completo e SHA uguale.
  - E6 downloader interrotto oltre la finestra: upload 256 MiB; download `curl --limit-rate 20M -o part.bin "$link" &`, kill dopo che `part.bin` supera 16 MiB; uploader exit 18 e output con `# failed:`; `curl -fsS "$link"` → 404.
  - E7 downloader interrotto dentro la finestra: upload 64 MiB; primo download `curl --limit-rate 50k -o /dev/null "$link" &` ucciso dopo 1 s; l'uploader stampa `# download interrupted before the first 4 MiB`; secondo download completo con SHA uguale; uploader exit 0.
  - E8 scadenza: secondo server con `BORE_FAST_LINK_TRANSFER_WAIT_TIMEOUT=2`; upload senza download; uploader exit 18 entro 10 s con `# expired:`; link → 404.
  - E9 uploader interrotto: `head -c 300M /dev/urandom | curl -sS -N -u u:$PASS -T - .../s.bin &` e download in parallelo; kill del curl uploader dopo che il downloader ha ricevuto ≥ 32 MiB; curl downloader exit ≠ 0 (18 atteso).
  - E10 `GET /` → 200 con `curl -u USER:PASS -T` nel corpo.
  - E11 HTTP in chiaro: `curl -sS -o /dev/null -w '%{http_code}' -u u:$PASS -T f.bin http://fast.bore.local:$HP/` con `--resolve fast.bore.local:$HP:127.0.0.1` → 403; GET `http://.../x` → 308 con `Location: https://`.
  - E12 kill-switch: server con `BORE_FAST_LINK_TRANSFER_ENABLED=false` e le altre env impostate → il log contiene `ignoring this setting` e `https://fast.bore.local:$SP/` risponde 502.
  Job CI `fast-link` (dopo `transfer-link-docker`, stesso stile): `runs-on: ubuntu-latest`, `timeout-minutes: 25`, checkout, rust stable, rust-cache, passi `bash scripts/fast_link_e2e.sh` e `bash scripts/fast_link_perf.sh` (quest'ultimo attivo da 2.2), `if: inputs.only != 'web-transfer'` come gli altri job.
  Steps:
  1. S1 — script con E1–E12; expected `bash scripts/fast_link_e2e.sh` → 12 PASS in locale.
  2. S2 — job CI (con il solo passo e2e finché 2.2 non esiste).
  Recovery boundary: none.
  Failure handling: un caso instabile per tempi (E6/E7/E9) → rendere l'attesa condizionale su un fatto osservabile (dimensione del file, riga nel log) con deadline, mai `sleep` fissi lunghi; bug del server scoperto → aprire la correzione nell'unità, test unitario che lo riproduce, poi fix.
- **Unit tests:** N/A — script di accettazione; eventuali bug trovati ottengono un test unitario nel modulo coinvolto.
- **e2e tests:** G-E2E (`bash scripts/fast_link_e2e.sh`): 12 righe `PASS`.
- **Done:** G-E2E verde in locale; job CI aggiunto; unità chiusa; commit di completamento.

### 2.2 `scripts/fast_link_perf.sh` — banda contro baseline vhost + prova di solo transito
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus DOPO il diff (validità della misura: bracci interleaved, stessa sorgente tmpfs, nessun disco nel percorso misurato, campioni grezzi stampati, `LC_ALL=C` per le mediane).
- **Files:** READ `scripts/transfer_link_perf.sh` (baseline vhost con origin Python e `bore vhost`, `measure_url`). NEW `scripts/fast_link_perf.sh`. WRITE `.github/workflows/ci.yml` (passo nel job `fast-link`).
- **Change:**
  Preconditions: 2.1 DONE.
  Contract: un solo server con vhost HTTPS + fast link (come nel setup comune) e `export BORE_PROXY_BUFFER_SIZE="${BORE_PROXY_BUFFER_SIZE:-16M}"` come la baseline esistente. Payload `payload.bin` 1 GiB da tmpfs.
  - Braccio VHOST (baseline): origin HTTP locale che serve `payload.bin` (copiare l'origin Python della baseline esistente SENZA l'hash, così nessun braccio fa lavoro extra), provider `bore vhost` sottodominio `perf` verso l'origin (TCP relay: nessun `--udp`), download `curl -s -o /dev/null -w '%{speed_download}' --cacert ... https://perf.bore.local:$SP/payload.bin`.
  - Braccio FAST: uploader `curl -s -N -u u:$PASS -T payload.bin https://fast.bore.local:$SP/payload.bin` (link dalla prima riga), downloader `curl -s -o /dev/null -w '%{speed_download}' "$link"`; misura = velocità del downloader.
  - 3 ripetizioni, ordine interleaved `VHOST FAST FAST VHOST VHOST FAST`, un warm-up non misurato per braccio. Stampa `T-FL-PERF raw vhost=<a,b,c> fast=<a,b,c>` (MiB/s, `LC_ALL=C`), mediane, rapporto. PASS se `median(fast) >= 1.0 * median(vhost)`.
  - Transito (T-FL-TRANSIT) durante la seconda run FAST: prima del download leggere `write_bytes` da `/proc/$server_pid/io` e `VmRSS` da `/proc/$server_pid/status`; campionare `VmRSS` a 10 Hz fino a fine download; dopo, rileggere `write_bytes`. PASS se `Δwrite_bytes ≤ 262144` e `max(VmRSS) − VmRSS_iniziale ≤ 48 MiB`. `$server_pid` = PID del processo `bore` (non della pipe: avviare con `"$BORE" server ... > >(cat >"$tmp/server.log") 2>&1 &`, poi `server_pid=$!`). Solo Linux: fuori da Linux lo script stampa `SKIP` ed esce 0.
  - CPU (informativo, non gate): `utime+stime` del server da `/proc/$server_pid/stat` prima/dopo ciascun braccio → `cpu_s_per_gib` stampato.
  Steps:
  1. S1 — script; expected in locale PASS con i numeri grezzi registrati in STATE.md §7.
  2. S2 — passo `bash scripts/fast_link_perf.sh` nel job CI `fast-link`.
  Recovery boundary: none.
  Failure handling: rapporto < 1.0 in locale → NON è un problema dello script: profilare (`perf top` o `cpu_s_per_gib` per braccio), riportare ad agent-1:opus con i campioni grezzi prima di toccare la soglia; il fix va nel motore (0.3) con test.
- **Unit tests:** N/A.
- **e2e tests:** G-PERF (`bash scripts/fast_link_perf.sh`): righe `T-FL-PERF PASS` e `T-FL-TRANSIT PASS`.
- **Done:** G-PERF verde in locale con numeri registrati; review agent-1:opus; passo CI aggiunto; unità chiusa; commit di completamento.

### 2.3 Download reale da browser (Playwright, Chromium)
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus a P2.
- **Files:** READ `web/transfer/tests/e2e/helpers.mjs` (`boreBin`, `freePort`, `waitPort`, registro processi `track`), `web/transfer/playwright.config.mjs`, `.github/workflows/ci.yml` job `web-transfer-e2e`. NEW `web/transfer/tests/e2e/fast-link.spec.mjs`. WRITE `web/transfer/tests/e2e/helpers.mjs` solo se serve esportare helper già esistenti (nessun cambiamento di comportamento).
- **Change:**
  Preconditions: P1 DONE (indipendente da 2.1/2.2).
  Contract: a livello di file `test.use({ ignoreHTTPSErrors: true, launchOptions: { args: ['--host-resolver-rules=MAP fast.bore.local 127.0.0.1'] } })`; `test.skip(({ browserName }) => browserName !== 'chromium', '--host-resolver-rules is a Chromium switch; the behaviour under test is standard HTTP download handling')`. Setup nel test: `mkdtemp`, `openssl` via `spawnSync` per CA+leaf `*.bore.local`, server `boreBin server` con le env fast link e vhost HTTPS su porta libera (processo registrato con `track`), payload 32 MiB casuale scritto su disco temporaneo del test, uploader `curl -sS -N -u u:p --cacert ca.pem --resolve fast.bore.local:<p>:127.0.0.1 -T payload.bin https://fast.bore.local:<p>/payload.bin` spawnato; prima riga stdout = link.
  Steps:
  1. S1 — T-FL-PW `a browser downloads a fast link once`: `const dl = page.waitForEvent('download'); await page.goto(link).catch(() => {});` (la navigazione su un attachment rifiuta con "Download is starting"); `download.suggestedFilename() === 'payload.bin'`; SHA-256 del file `await download.path()` == payload; il processo uploader esce con codice 0; un secondo `page.request.get(link)` → 404.
  2. S2 — verifica locale: `npm run build --prefix web/transfer` (nessun cambiamento atteso in dist), `cargo build --all-features --bin bore --example web_transfer_e2e_owner`, poi dalla directory `web/transfer`: `npx playwright test --project=chromium fast-link`.
  Recovery boundary: none.
  Failure handling: `download` non emesso → controllare header `Content-Disposition: attachment` (bug server → test unitario + fix); lo spec non deve toccare il resto della suite.
- **Unit tests:** N/A.
- **e2e tests:** G-PW: 1 test passato su chromium, skip motivato su firefox/webkit nella matrice CI esistente.
- **Done:** G-PW verde in locale; nessun file `dist` modificato; unità chiusa; commit di completamento.

### 2.4 Documentazione finale
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus a P2 (accuratezza contro il codice e gli script).
- **Files:** WRITE `README.md` (sezione "Fast link transfer"), `docs/README.md` (indice), `CLAUDE.md` (sezione "Key invariants", un bullet nuovo). NEW `docs/transfer/FAST_LINK.md`.
- **Change:**
  Preconditions: 2.1, 2.2, 2.3 DONE.
  Steps:
  1. S1 — `docs/transfer/FAST_LINK.md`: scopo e scenario; perché è sempre relay (nessun client = nessun hole-punch: uploader→server→downloader, TLS terminato sul server, un salto senza mux); flusso e macchina a stati (D13); tabella delle risposte (D9, D10, D12, D14 con codici e testi); framing (D11); anti-anteprima (D2) con la lista UA e i limiti (scanner che scaricano tutto); pompa a due task e perché (D8); limiti D15 e formula RAM (I-2); sicurezza (D1, D18, D21, D9); admin (D6); invarianti I-1..I-11 con i test che li custodiscono; numeri di T-FL-PERF registrati in 2.2 con data e macchina.
  2. S2 — README: completare la sezione di 1.3 con i codici di uscita verificati da 2.1, la nota `-N`, la tabella troubleshooting (401 credenziali, 403 HTTP in chiaro, 404 link scaduto/usato, 409 download in corso, 416 richiesta Range, 503 troppi upload, link non stampato → `-N`), link a `docs/transfer/FAST_LINK.md`. `docs/README.md`: voce per `FAST_LINK.md`.
  3. S3 — `CLAUDE.md` "Key invariants": un bullet `**Fast link transfer (docs/transfer/FAST_LINK.md, plan 004).**` di 8–12 righe: solo HTTPS; label riservato via `ReservedVhostLabel` (mai campo in `VhostConfig`); auth solo sulla head e prima del 100; un download per upload, re-arm solo con replay integro (4 MiB); successo = terminatore, ogni altro esito chiude senza (curl 0/18); pompa a due task con buffer riciclati, mai `tokio::io::copy`, flush dopo ogni write; `ConnSecurity` statico sul tipo per non toccare il loop di accept (I-SSH1); gate T-FL-PERF/T-FL-TRANSIT.
  Recovery boundary: none.
  Failure handling: discrepanza doc/codice → il codice e i test sono la verità; se il codice è sbagliato aprire la correzione nell'unità responsabile.
- **Unit tests:** N/A — documentazione.
- **e2e tests:** N/A — i comandi documentati sono quelli eseguiti da 2.1.
- **Done:** documenti scritti e verificati contro `git grep` dei nomi flag/env e contro l'output di 2.1; unità chiusa; commit di completamento.

## Phase gates and closure
- Required gates: G-FMT, G-CLIPPY, G-FULL, G-NODEF, G-I1, G-NPM, G-E2E, G-PERF, G-PW, poi G-FINAL (push su `dev` e CI remota verde su tutti i workflow del commit).
- README obligation: sezione "Fast link transfer" completa (2.4 S2) e coerente con E1–E12.
- Scenario di riferimento: E1 (file), E2 (tar streaming + wget), T-FL-PW (browser).
- Aprire P2: gate locali completi, review agent-1:opus finale, fase DONE, commit di chiusura; poi push su `dev` (autorizzato dall'utente il 2026-09-25: "alla fine push della funzionalità completa su dev, con ci verde") e monitoraggio CI fino al verde; un fallimento CI riapre l'unità responsabile.
