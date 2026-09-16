# Phase 4 — WebRTC diretto con fallback automatico

> **Intent:** rendere WebRTC DataChannel il percorso predefinito e ripiegare automaticamente sul relay cifrato per lo stesso trasferimento logico.
> **Shippable alone?** yes — completa il requisito direct-first per i file singoli e mantiene la slice relay già funzionante.
> **Preconditions:** Phase 3 DONE

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`, finish or revert that unit first (§6 says how far it got). Run the gate commands in STATE.md **§3** and check the result against what §1, §7, and §11 claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type: sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to `claimed — nothing written yet`. Write or update the listed tests first or alongside production edits; do not defer them to a later unit.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1 at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP commits are on.

---

## Fixed contracts for this phase

> **User-visible default change:** after this phase, `bore transfer web` no longer starts payload on relay immediately after a click. It always attempts WebRTC for 10 seconds first and falls back automatically; existing relay-only tests must be updated to assert the new negotiation while all legacy non-web modes remain byte-identical.

- Ogni nuovo trasferimento tenta WebRTC prima di acquisire un permit relay. Non esiste flag utente per saltare il diretto o forzarlo.
- Una singola `RTCPeerConnection` appartiene a un singolo TransferId/AttemptId. Non condividere peer connection o DataChannel fra trasferimenti.
- Il destinatario è sempre SDP offerer e chiama `createDataChannel`; la fonte è sempre answerer e usa `ondatachannel`. Canale: label `bore-transfer-v1`, `ordered:true`, protocol `bore-transfer-v1`, `binaryType="arraybuffer"`.
- Il server inoltra signaling soltanto ai due partecipanti dell'attempt corrente e non conserva SDP/candidate dopo l'invio. Non logga né pubblica questi valori.
- Deadline diretto: 10 secondi da `transfer.direct_start` fino a `direct_ready` di entrambi. Qualsiasi timeout/failure/close prima del completamento invalida l'attempt e crea automaticamente un nuovo attempt relay con chiave nuova.
- Soltanto `transfer.path_commit` autorizza la fonte a leggere file/inviare payload. Il server lo emette per `direct` dopo entrambi i ready o per `relay` dopo il pairing.
- Il path mostrato/registrato diventa `direct` o `relay` soltanto quando il destinatario ha verificato il primo chunk e lo dichiara in `transfer.progress`; fino ad allora è `connecting`.
- Il fallback conserva TransferId, selectionDigest e partial OPFS, ma cambia AttemptId, attempt number, chiave, nonce sequence e trasporto. I frame tardivi del vecchio attempt vengono ignorati e mai scritti.

## Sub-phases

### 4.1 Estendere la macchina server con negoziazione e signaling WebRTC

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** aggiungere stati direct e relay fallback mantenendo autorità server e limiti esatti; fare self-review di ogni race signaling/timeout/cancel.
- **Files:** `src/web_transfer.rs:TransferState — direct states, signaling counters and fallback`; `src/web_transfer_protocol.rs:Rtc payloads — direct control bodies`; `src/web_transfer_http.rs:control dispatch — signaling messages`; `tests/web_transfer_test.rs:signaling state group — roles, bounds and fallback`.
- **Change:** dopo source_ready, creare attempt 1 e passare a `NegotiatingDirect {started_at,ready_source:false,ready_recipient:false,candidates_source:0,candidates_recipient:0}`; inviare `transfer.direct_start {transferId,attemptId,attemptNumber,role,iceServers,deadlineMs:10000}` ai due peer, con ruolo recipient `offerer` e source `answerer`. Accettare `rtc.offer` soltanto dal recipient, una volta, SDP UTF-8 1..65536 byte e type `offer`; inoltrarla senza conservarla. Accettare `rtc.answer` soltanto dalla fonte dopo offer, una volta, type `answer`, stesso limite. Accettare `rtc.ice` da entrambi per attempt corrente, candidate string <=4096 byte, `sdpMid` <=64, line index u16/null, oppure marker end-of-candidates; massimo 128 per lato, inoltro immediato. Non analizzare/riscrivere SDP/candidate e non includerli in errori/log. `transfer.direct_ready` è valido soltanto dal partecipante corrispondente dopo signaling e imposta il bit; al secondo ready inviare path_commit direct a recipient e poi source, con send bounded e senza lock. Un timer cattura `Weak<Room>`, TransferId e AttemptId; a 10 s chiama `fallback_to_relay` solo se pointer, state e attempt coincidono. `transfer.direct_failed` da uno dei partecipanti prima/comunque durante ActiveDirect invalida l'attempt una volta e inoltra al counterpart una reason code fissa più resume ranges bounded. `fallback_to_relay` crea nuovo AttemptId/number e usa esattamente l'admission/ticket flow Phase 3; se relay busy lascia stato retryable e UI esplicita. Direct non acquisisce il semaforo relay. Cancel/withdraw/disconnect/room close vincono su timer e signaling. Nessun monitor risolve una room corrente per ID: usa Weak/epoch. R1 — WebRTC usa signaling applicativo per scambiare descrizioni e candidate ([W3C WebRTC](https://www.w3.org/TR/webrtc/)); R4 — ICE seleziona coppie di candidate per traversal NAT ([RFC 8445](https://www.rfc-editor.org/rfc/rfc8445.html)).
- **Unit tests:** `recipient_is_fixed_offerer_and_source_fixed_answerer`; `sdp_order_roles_sizes_and_singletons_are_enforced`; `ice_candidates_are_bounded_per_side_and_end_marker_forwards`; `signaling_is_forward_only_and_never_logged`; `both_ready_commit_direct_recipient_then_source`; `direct_timeout_falls_back_once_with_fresh_attempt`; `direct_failure_mid_active_preserves_transfer_and_ranges`; `relay_busy_after_direct_failure_is_retryable`; `stale_direct_timer_and_messages_cannot_touch_new_attempt`; `cancel_disconnect_withdraw_win_over_fallback`; `direct_never_acquires_relay_permit`.
- **e2e tests:** `T-WEB-SIGNALING` — due control peer completano offer/answer/candidate/ready nell'ordine valido; ruolo errato, oversize, 129° candidate, stale attempt e timeout sono respinti o fanno fallback senza corrompere il transfer.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-SIGNALING` passa + race tests con clock controllato non mostrano doppio commit/permit leak + self-review traccia tutte le transizioni direct→relay/terminali + closed in `STATE.md` (§1 → 4.2, §4 ledger row, §6 `none`, §11 board).

- **Esito (2026-09-16):** `transfer.source_ready` non tocca più il semaforo relay.
  Porta il trasferimento in `TransferState::NegotiatingDirect {started_at,
  ready_source, ready_recipient, candidates_source, candidates_recipient,
  offer_seen, answer_seen}` e manda `transfer.direct_start` prima al recipient
  (`role:"offerer"`, è lui che crea l'unico DataChannel) e poi alla source
  (`role:"answerer"`), con `iceServers` presi da `IceServerConfig` (solo URL
  `stun:`) e `deadlineMs` letto dal deadline in vigore. **Due deviazioni dal
  testo della sub-fase, entrambe additive:** i campi `offer_seen`/`answer_seen`
  non erano nell'elenco del piano ma senza di essi il vincolo "una volta sola,
  answer dopo offer" non è esprimibile; e `role` vale `offerer`/`answerer`
  invece dell'`"recipient"` che compariva nella fixture — il valore dice al
  browser cosa fare, non chi è, e la fixture è stata allineata. Il forwarding è
  sola andata: `signal_slot` valida ruolo, attempt corrente, stato e budget, poi
  `rtc_sdp_envelope`/`rtc_ice_envelope` ricostruiscono l'envelope per il
  counterpart senza il `requestId` del mittente. SDP e candidate non vengono mai
  letti: il parser controlla solo la lunghezza (1..65536 byte, candidate 4096,
  `sdpMid` 64, `sdpMLineIndex` u16) e `signaling_is_forward_only_and_never_logged`
  verifica che il `Debug` dell'intero record non contenga nessuno dei due
  marcatori e che nemmeno un rifiuto li citi. Il marker end-of-candidates ha una
  sola forma sul filo: chiave assente, `null` o stringa vuota diventano
  `candidate: null` e non possono portare altri campi.
  **`rtc.ice` è deliberatamente fuori dal mutation bucket** (4/s, burst 8): il
  trickle ICE emette i candidate a raffica mentre il gathering li trova, e
  limitarli a quattro al secondo blocca proprio l'handshake che il bucket
  dovrebbe proteggere. Resta il control bucket (30/s, burst 60) e soprattutto il
  budget di 128 per lato, che è il vincolo vero. `T-WEB-SIGNALING` manda i 128
  candidate reali e si dà il passo oltre il burst, così misura il budget e non
  il bucket.
  Al secondo `transfer.direct_ready` distinto lo stato diventa `ActiveDirect` e
  il `path_commit direct` parte verso recipient e poi source; `complete_transfer`
  accetta ora entrambi gli stati attivi (`is_carrying`). Il timer cattura
  `Weak<WebTransferRoom>` (regola P-14: un monitor non risolve mai una chiave a
  scadenza) più TransferId e AttemptId, e `fallback_to_relay` cambia qualcosa
  solo se stato e attempt coincidono ancora: è quello che fa convergere timer,
  `transfer.direct_failed` tardivo e terminale su UN solo fallback. Il fallback
  conia AttemptId/numero nuovi, conserva TransferId, selectionDigest e i verified
  range (creduti solo dal recipient — è l'unico che sa cosa ha scritto) e riusa
  identico il flusso di admission/ticket della Phase 3.
  **Un gate red-checked per parte:** togliere il confronto sull'attempt in
  `fallback_to_relay` fa fallire `stale_direct_timer_and_messages_cannot_touch_new_attempt`
  (`left: Queued` — il fallback tocca l'attempt sbagliato), togliere il controllo
  di ruolo sull'offer fa fallire `sdp_order_roles_sizes_and_singletons_are_enforced`.
  **Costo sul resto del piano:** i test che volevano il relay adesso ci arrivano
  rifiutando il diretto, come farà un browser senza DataChannel — helper
  `ready_then_relay` (unit) e `support::decline_direct` (e2e Rust). Lato browser
  `main.js` risponde a `transfer.direct_start` con `transfer.direct_failed
  {reason:"unsupported"}`: è uno stub esplicito che 4.2 sostituisce con l'attore
  vero, e serve perché senza di esso ogni transfer del suite aspetterebbe i 10 s
  del deadline. Il sender adotta l'AttemptId del ticket quando questo nomina un
  attempt diverso e non ha ancora mandato un byte — è così che la chiave derivata
  è sempre quella nuova.
  **Gate:** fmt, clippy `-D warnings`, `cargo build --all-features`,
  `cargo test --all-features --lib` 782 passed / 0 failed,
  `cargo test --all-features --test web_transfer_test -- --test-threads=1`
  20 passed (incluso il nuovo `t_web_signaling`), `npm run check` 92/92,
  `npm run test:e2e` 78 passed su chromium+firefox+webkit.

### 4.2 Implementare RTCPeerConnection e DataChannel nei browser

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** implementare gli attori WebRTC con ruoli fissi e limiti browser; fare self-review degli handler, cleanup e backpressure.
- **Files:** `web/transfer/src/webrtc.js:new — peer connection and signaling actor`; `web/transfer/src/main.js:attempt orchestration — direct wiring`; `web/transfer/src/control.js:rtc dispatch — signaling routing`; `web/transfer/src/sender.js:EncryptedFrameSink — DataChannel adapter`; `web/transfer/src/receiver.js:EncryptedFrameSource — DataChannel adapter`; `web/transfer/tests/unit/webrtc.test.mjs:new`; `web/transfer/tests/e2e/direct.spec.mjs:new`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** costruire `RTCPeerConnection({iceServers})` soltanto dopo `transfer.direct_start`, filtrando config server a URL `stun:` già validate e senza TURN credentials. Recipient crea subito un solo DataChannel con contratto fisso, installa handler, crea/setta local offer e invia rtc.offer; source setta remote offer, riceve esattamente quel channel via `ondatachannel`, crea/setta local answer e invia rtc.answer. Entrambi inoltrano trickle candidate e marker finale, accettano remote candidate soltanto dopo remote description usando una coda locale massima 128 e poi la svuotano. SDP/candidate non vanno in console/error UI. Considerare ready soltanto quando connectionState non è failed/closed, channel `open`, label/protocol/order corretti e `pc.sctp.maxMessageSize` consente almeno 1024 byte. Calcolare frammento plaintext per questo attempt come `min(24576, maxMessageSize - 64)`, con floor 1024; il path relay mantiene 24576. Inviare direct_ready una volta. Impostare `bufferedAmountLowThreshold=1048576`; se bufferedAmount supera 4194304, sospendere future File reads e attendere `bufferedamountlow`, AbortSignal o timeout 10 s. Ricevere solo ArrayBuffer; text/Blob non convertibile, messaggio oversized, channel aggiuntivo o signaling duplicato produce direct_failed. Ogni actor possiede listener cleanup e `close()` idempotente che rimuove callback, chiude data channel e pc. `failed`, `closed`, `disconnected` persistente per 2 s o send error prima completion invia direct_failed una volta. `disconnected` recuperato entro 2 s non fa fallback. Nessuna chiamata getUserMedia/permission. R1 — API e stato RTCPeerConnection/DataChannel ([W3C](https://www.w3.org/TR/webrtc/)); R2 — canale ordinato/affidabile e message boundaries ([RFC 8831](https://www.rfc-editor.org/rfc/rfc8831.html)); R3 — rispettare `max-message-size` del peer ([RFC 8841](https://www.rfc-editor.org/rfc/rfc8841.html)); R11 — `bufferedAmountLowThreshold` segnala il drenaggio ([MDN](https://developer.mozilla.org/en-US/docs/Web/API/RTCDataChannel/bufferedAmount)).
- **Unit tests:** `receiver_creates_exactly_one_ordered_reliable_channel`; `source_never_creates_channel_and_accepts_only_expected_one`; `offer_answer_and_candidates_follow_fixed_roles`; `remote_candidates_queue_is_bounded_until_description`; `ready_requires_open_valid_channel_and_min_message_size`; `fragment_size_respects_negotiated_max`; `high_low_water_pauses_before_next_file_read`; `transient_disconnect_has_two_second_grace`; `failure_is_reported_once_and_cleanup_is_idempotent`; `no_media_or_permission_api_is_called`.
- **e2e tests:** `T-WEB-DIRECT` — browser A/B stabiliscono DataChannel reale, il primo file read avviene dopo path_commit, bytes verificati sono esatti e nessuna `/relay/` socket viene aperta; eseguire almeno Chromium↔Chromium e Firefox↔Firefox, più WebKit quando il runner supporta WebRTC loopback.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-DIRECT` passa sui progetti supportati senza fake del DataChannel + instrumentation conferma una connection per transfer e zero relay + self-review verifica rimozione di tutti gli event listener e bounded candidate queue + closed in `STATE.md` (§1 → 4.3, §4 ledger row, §6 `none`, §11 board).

**Esito (2026-09-16).** 4.2 implementato e chiuso.

- **`web/transfer/src/webrtc.js` (nuovo)** — `createAttemptRtc({role, transferId,
  attemptId, iceServers, sendSignal, createPeerConnection, events})`: un solo
  `RTCPeerConnection` e un solo DataChannel per `(transferId, attemptId)`,
  costruiti SOLO dopo `transfer.direct_start`. `filterIceServers` tiene solo
  `stun:`/`stuns:` e butta ogni credenziale (difesa in profondità: il server già
  valida, ma questo è l'unico punto in cui un URL TURN potrebbe entrare nella
  pagina). `fragmentBytesFor` = `min(24576, maxMessageSize - 64)` con floor 1024;
  sotto il floor il canale è **inutilizzabile** (`unsupported`), non un frammento
  più piccolo. Ready richiede: canale `open`, label/protocol/ordered esatti,
  dimensione sopra il floor, `connectionState` non `failed`/`closed` **e il
  proprio passo SDP completato** — il server rifiuta un `direct_ready`
  prematuro, quindi mandarlo brucerebbe l'attempt.
- **Ruoli** letti dal server e mai derivati: `role === "offerer"` ⇒ questo tab è
  il DESTINATARIO, crea il canale e manda `rtc.offer`; `answerer` ⇒ è la FONTE,
  non chiama mai `createDataChannel` e accetta esattamente un canale via
  `ondatachannel`. Un secondo canale sulla stessa connessione è `protocol`.
- **Trickle ICE**: ogni candidato locale parte subito; il marker di fine
  gathering viaggia come `{candidate: null}`, la UNICA forma che il server
  normalizza. I candidati remoti arrivati prima della remote description stanno
  in coda **limitata a 128** (lo stesso budget che il server applica) e la coda
  si svuota appena la description è impostata.
- **Backpressure (BW-F3 portato nel browser)**: `bufferedAmountLowThreshold`
  1 MiB, e sopra 4 MiB accodati il mittente **aspetta `bufferedamountlow`**
  invece di accodare oltre — una coda profonda compra throughput e paga latenza,
  e qui la coda vive nella heap del tab. Un canale che non drena mai non parcheggia
  la pipeline per sempre: 10 s di timeout e il `send` successivo decide.
- **Un solo transport nel codice**: `sender.js` scrive su un `sink`
  (`{fragmentBytes, highWater, bufferedAmount, send, waitLow, close}`) che è il
  WebSocket del relay o il DataChannel; `receiver.js` riceve da `deliverFrame`
  qualunque sia la sorgente. `framing.js:fragmentWindow` prende la dimensione come
  parametro (default 24 KiB, che resta il CEILING su ogni percorso).
- **Fine-stream**: sul relay la chiusura del socket È il marker di fine; sul
  DataChannel lo è il frame FINAL, quindi la fonte NON chiude il canale dopo
  FINAL (chiuderlo correrebbe contro i byte ancora in coda). L'attore viene chiuso
  ai terminali (`onStaged`/`onDone`/`onCancelled`/`onError`/`room.closed`/cancel).
- **Un canale che si chiude dopo l'ultimo byte non è un guasto**: prima di mandare
  `transfer.direct_failed` il main chiede all'attore di trasferimento se l'attempt
  è ancora vivo (`receiver.directFailed` torna `null` su `complete-pending`/`staged`,
  `sender.detachDirect` torna `false` su `done-pending`). Senza questo controllo ogni
  trasferimento diretto riuscito chiedeva un attempt relay che nessuno voleva —
  trovato dal gate, non ragionato.
- **Attempt scoping lato fonte**: `transfer.attemptAbort` è un SECONDO
  `AbortController`, per attempt. Abbandonare il diretto ferma la sua pipeline e
  NON cancella il trasferimento, che prosegue sull'attempt relay. Il progresso non
  torna indietro attraverso il fallback: i chunk già verificati contano come
  inviati (`baseBytes`) e FINAL conta solo i byte di QUESTO attempt.
- **Attempt scoping lato destinatario**: `adoptAttempt` ricostruisce chiave,
  sequenza da zero, piano e `expectedBytes` dai range **verificati su disco** in
  quel momento; i byte di un chunk a metà vengono buttati (solo un chunk intero e
  digest-verificato è su disco). `transfer.relay_ticket` con un attemptId diverso è
  esattamente il fallback e passa di qui.

**Deviazioni dichiarate**

1. *Il piano elencava `control.js:rtc dispatch`* — il routing sta in `main.js`,
   dove già vive tutto il dispatch dei messaggi di trasferimento; `control.js`
   resta il trasporto puro che non conosce nessun tipo applicativo. Spostarlo là
   avrebbe creato un secondo punto di conoscenza degli attempt.
2. *Il piano non prevedeva un fixture per i gate relay esistenti* — con il
   diretto come default, ogni e2e relay di Phase 3 sarebbe andato in diretto.
   `fixtures.js:disableWebRtc` presenta il contesto come un motore **senza**
   WebRTC (caso reale e supportato: WebRTC disabilitato da policy), così i gate
   relay misurano ancora il relay e lo fanno subito, senza consumare la deadline.
   `forceIceRelayOnly` (il wrapper che 4.4 richiede) è scritto qui accanto ma
   non ancora usato.
3. *`MAX_INBOUND_BYTES` 64 KiB* — non era nel piano: è il tetto sul messaggio in
   ingresso, sopra il quale l'attempt finisce. Serve perché il frammento nostro è
   ≤ 24 KiB + header + tag, e un peer che manda di più non sta parlando questo
   protocollo.

**Gate**

- `cargo fmt --all -- --check` OK, `cargo clippy --all-features --all-targets -D warnings` pulito, `cargo build --all-features` OK.
- `cargo test --all-features --lib`: 782 passed / 0 failed / 2 ignored.
- `cargo test --all-features --test web_transfer_test -- --test-threads=1`: 20 passed / 0 failed / 1 ignored (113 s).
- `npm run check --prefix web/transfer`: **105/105** (+13 di `webrtc.test.mjs`).
- `npm run test:e2e --prefix web/transfer`: **84 passed / 0 failed** su chromium + firefox + webkit (+6: i due leg di `direct.spec.mjs` per motore).
- `T-WEB-DIRECT` verde su tutti e tre i motori con DataChannel REALE (nessun fake): percorso `direct` committato da entrambe le parti, zero socket `/transfer/ws/relay/`, `rtcConstructed === 1` per pagina, nessun byte di payload letto fra `transfer.incoming` e `transfer.path_commit`, hash del file salvato identico.
- Due red-check eseguiti e ripristinati: togliendo il guard "il proprio passo SDP è fatto" fallisce `ready_requires_open_valid_channel_and_min_message_size`; togliendo il budget di 128 candidati fallisce `remote_candidates_queue_is_bounded_until_description`.

**Costo pagato dal resto del piano**

- Il blocco temporaneo di 4.1 in `main.js` è **eliminato**, come previsto.
- `support::decline_direct` (Rust e2e) e `ready_then_relay` (unit) restano: un
  test Rust non ha un `RTCPeerConnection` e deve continuare a rifiutare il
  diretto esplicitamente.
- 4.3 eredita `EncryptedFrameSink`/`EncryptedFrameSource` già di fatto esistenti
  (il `sink` del sender e `deliverFrame` del receiver) e `adoptAttempt`/
  `detachDirect`, quindi gli resta il `TransferController` unico, il fallback a
  metà trasferimento con range forwarding e la UI `connecting → direct|relay`.

---

### 4.3 Integrare commit, fallback iniziale e ripresa dopo guasto diretto

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** unificare sender/receiver sui due transport adapter e implementare fallback senza doppio invio; self-review attempt isolation e persistenza OPFS.
- **Files:** `web/transfer/src/main.js:TransferController — attempt coordinator`; `web/transfer/src/sender.js:EncryptedFrameSink — common RTC/WS output`; `web/transfer/src/receiver.js:EncryptedFrameSource — common RTC/WS input`; `web/transfer/src/storage.js:verifiedRanges — resume handoff`; `web/transfer/src/webrtc.js:AttemptRtc — lifecycle`; `web/transfer/src/control.js:attempt messages — routing`; `web/transfer/tests/unit/state.test.mjs:fallback cases`; `web/transfer/tests/e2e/direct.spec.mjs:fallback cases`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** introdurre un solo `TransferController` per TransferId e un `AttemptController` sostituibile. Ogni callback cattura AttemptId e prima di mutare UI/storage confronta l'attempt corrente; frame tardivi vengono scartati prima del decrypt/write. Il sender usa una interfaccia `EncryptedFrameSink {ready,bufferedAmount,waitLow,send,close}` implementata da DataChannel e WebSocket; il receiver usa `EncryptedFrameSource` equivalente. Il path_commit deve corrispondere all'attempt e al transport pronto; soltanto allora il controller avvia sender/receiver. Su timeout iniziale, failure esplicita o unsupported WebRTC, chiudere RTC, abbandonare chiave e attendere i ticket relay automatici senza nuovo click. Su guasto dopo uno o più chunk verificati, il receiver flush/committe l'ultimo chunk completo, costruisce ranges bounded, invia direct_failed, ignora frammenti incompleti e attende attempt relay. Source ricalcola anche i chunk saltati e invia solo mancanti. Mai continuare con sequence del vecchio attempt. UI resta una riga TransferId e aggiorna `connecting → direct` oppure `connecting → relay`; path diventa visibile solo dopo il primo `CHUNK_DIGEST` verificato e progress report del recipient. Se direct completa mentre un failure è in volo, il server terminal Completed vince e nessun relay viene allocato; se cancel vince, entrambi gli attempt chiudono. Limitare a un fallback automatico direct→relay per request; un relay fallito richiede click Riprendi, evitando loop automatici. Progress è monotono sui verified bytes attraverso gli attempt.
- **Unit tests:** `path_commit_is_required_and_attempt_bound`; `old_attempt_frames_callbacks_and_keys_are_ignored`; `initial_direct_failure_opens_one_relay_without_new_request`; `mid_direct_failure_commits_only_complete_chunk_and_forwards_ranges`; `relay_attempt_uses_fresh_key_nonce_and_sequence`; `source_rehashes_skipped_ranges`; `ui_keeps_one_transfer_and_monotonic_progress`; `first_verified_chunk_is_only_path_authority`; `completion_cancel_failure_race_has_one_winner`; `failed_relay_never_auto_loops`.
- **e2e tests:** `T-WEB-DIRECT-FALLBACK` — chiudere DataChannel dopo 25%, verificare stessa TransferId, nuovo AttemptId/chiave, relay automatico, skip dei chunk OPFS verificati e file finale esatto; `T-WEB-DIRECT-TIMEOUT` — ICE fallisce prima di payload e relay parte automaticamente senza secondo click; `T-WEB-NOAUTO` — ancora zero RTC prima del click.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + i tre gate passano su Chromium/Firefox e il fallback su WebKit + ciphertext fixture prova chiavi diverse fra attempt + self-review segue byte/sequence/offset attraverso il cambio transport senza duplicati scritti + closed in `STATE.md` (§1 → 4.4, §4 ledger row, §6 `none`, §11 board).

**Esito (2026-09-16).** 4.3 implementato e chiuso. Tre difetti reali trovati
dai gate nuovi, tutti sul percorso di fallback, tutti descritti sotto.

- **`transfer.progress` (nuovo messaggio, additivo in coda a `SERVER_TYPES`).**
  La fonte non può leggere il path dal proprio socket e non può contare ciò che
  l'altro capo ha VERIFICATO: `sentBytes` dice cosa è uscito, non cosa è stato
  controllato. Il destinatario manda `transfer.progress {transferId, attemptId,
  receivedBytes}` (stringa decimale, come ogni quantità a 64 bit su questo filo)
  **solo per un chunk verificato**, con throttle 500 ms / 1 MiB;
  `WebTransferRegistry::report_progress` lo inoltra alla SOLA fonte aggiungendo
  `path`, che è **del server** (`ActiveDirect ⇒ direct`, altrimenti `relay`) e
  mai un valore di un peer — un peer dice cosa ha verificato e nient'altro.
  Rifiuti: la fonte è `INVALID_MESSAGE` ("only the recipient reports"), un
  estraneo `NOT_PARTICIPANT`, oltre `entry_size` è `INVALID_MESSAGE`; un attempt
  stale, uno stato non-carrying e `receivedBytes: 0` sono ack senza inoltro.
  L'arm HTTP è **fuori dal bucket mutation** per la stessa ragione di `rtc.ice`:
  è un report periodico su un trasferimento vivo e 4/s farebbe ritardare la
  vista della fonte su un trasferimento veloce; restano il bucket control
  (30/s, burst 60) e il fatto che il report non muta nulla che il server tenga.
- **Il badge di path ha una sola autorità.** Sul destinatario è il PRIMO chunk
  verificato dell'attempt corrente (`pathAttempt`), non il commit: un path
  committato che non ha ancora portato niente non è un fatto. Sulla fonte è
  esclusivamente il `transfer.progress` inoltrato dal server. La riga UI resta
  UNA per TransferId, `doneBytes` è monotono (`Math.max` nel reducer) e
  `transfer.path` accetta solo `direct`/`relay` — niente torna a `connecting`.
- **DIFETTO 1 (4.2, trovato da T-WEB-DIRECT-FALLBACK): il nome sul filo.**
  `main.js` mandava le range verificate come `verifiedRanges`; il server accetta
  `resumeRanges` e **rifiuta una chiave di body sconosciuta**, quindi l'intero
  `transfer.direct_failed` tornava `INVALID_MESSAGE`. Effetto: il fallback
  partiva lo stesso (la fonte riportava il suo), ma senza range — il relay
  rimandava tutto e il destinatario, che pianificava su ciò che aveva, leggeva
  il primo chunk come *digest mismatch*. Il nome vive ora in
  `protocol.js:directFailedBody` accanto a tutti gli altri, con il gate
  `direct_failed_ranges_travel_under_the_name_the_server_accepts` contro il
  corpus `transfer.direct_failed.ranges`.
- **DIFETTO 2: la fonte vince la corsa, e non è creduta sulle range.** La fonte
  scopre il canale morto alla sua prossima `send`, che è sincrona; il
  destinatario lo scopre da un evento `close`. Nel caso ordinario riporta prima
  la FONTE, e una fonte non è mai creduta sulle range (D3) — quindi l'unica
  informazione di resume esistente veniva buttata. Due metà:
  `TransferRecord.last_direct_attempt` + `adopt_late_recipient_resume` fanno sì
  che il report del destinatario **arrivato dopo** aggiorni comunque
  `record.resume`, ma solo per l'attempt che è davvero fallito e solo finché il
  rimpiazzo è ancora `WaitingRelay` (le range viaggiano su `path_commit`, non
  ancora inviato: niente che la fonte sappia già può cambiare sotto);
  e `applyCommitPlan` sul destinatario, che **pianifica dal commit** invece che
  dal proprio disco. Il `path_commit` è l'unico messaggio che entrambi i peer
  ricevono: prenderlo come autorità rende impossibile per costruzione che i due
  capi siano in disaccordo su quale chunk è sul filo. Ciò che è su disco non si
  perde (`verifiedRanges` non viene toccato): un chunk rimandato è semplicemente
  riverificato e riscritto.
- **DIFETTO 3: una `send` su canale morto uccideva il trasferimento.**
  `sink.send` lanciava un errore generico; `beginSend` lo leggeva come guasto
  del TRASFERIMENTO, chiamava `forget`, e il `transfer.relay_ticket` che
  arrivava subito dopo cadeva su un record che non esisteva più — la fonte non
  apriva mai la sua gamba relay. Ora `sink.send` riporta `send-error` **una
  volta** e lancia un `AbortError`, che il sender legge già come "questo attempt
  è stato abbandonato": il trasferimento resta e aspetta l'attempt relay.
- **Isolamento degli attempt, reso strutturale.** Il pump del destinatario
  cattura il proprio `attemptId` e un flag `attemptClosed` (alzato da
  `directFailed` PRIMA di leggere le range, così ciò che il server sente è
  esattamente ciò attorno a cui il destinatario pianificherà) e li ricontrolla
  **dopo ogni await**, anche dentro `acceptData`: un batch già in volo quando
  l'attempt muore non scrive un chunk, non avanza `planPos` e non estende le
  range dell'attempt che l'ha sostituito. Stessa regola sul socket relay: il suo
  `onmessage`/`onclose` è legato all'attempt che lo ha aperto. Sulla fonte
  l'`AbortController` per-attempt (4.2) resta l'unica cosa che ferma la
  pipeline senza cancellare il trasferimento.
- **Gate.** Unit JS 117/117 (nuovo `attempt.test.mjs` con gli 11 casi del piano
  — i 10 richiesti più `the_path_commit_is_the_only_plan_authority` — più
  `a_write_to_a_dead_channel_ends_the_attempt_not_the_transfer` in
  `webrtc.test.mjs` e `direct_failed_ranges_travel_under_the_name_the_server_accepts`
  in `protocol.test.mjs`). Rust lib 784/0/2 (i due nuovi
  `progress_is_recipient_only_and_bounded_by_the_entry` e
  `forwarded_progress_path_is_the_servers_and_goes_only_to_the_source`).
  Rust web e2e 20/0/1 in 112.7 s. Browser e2e **90/0** su chromium+firefox+webkit
  (erano 84: +T-WEB-DIRECT-FALLBACK e +T-WEB-DIRECT-TIMEOUT × 3 motori).
  `cargo fmt --check` e `cargo clippy --all-features --all-targets -D warnings`
  puliti. Regressione completa non-netns 30 suite, 1173 passati / 0 falliti /
  3 ignorati: la sua PRIMA corsa ha preso `t_web_readme`, che fissava ancora
  lo scope relay-only che il README aveva appena superato — il gate ha fatto
  esattamente il suo lavoro e le asserzioni si sono spostate con il testo
  (file singolo, direct-first, fallback relay automatico, cartelle ancora
  fuori), red-check incluso. Corpus fixture a 49 voci (`transfer.progress`,
  `transfer.progress.forwarded`, `transfer.direct_failed.ranges`), letto sia da
  Rust sia da JS.
- **Red-check** (fatti e ritirati): togliere il confronto di attempt in
  `sender.js:path_commit` fa cadere `path_commit_is_required_and_attempt_bound`;
  togliere `Math.max` da `doneBytes` fa cadere
  `ui_keeps_one_transfer_and_monotonic_progress`; fissare `path = "direct"` in
  `report_progress` fa cadere
  `forwarded_progress_path_is_the_servers_and_goes_only_to_the_source`.
- **Deviazioni dichiarate.** (a) `T-WEB-DIRECT-FALLBACK` chiude il canale dal
  DESTINATARIO avvolgendo `RTCPeerConnection.prototype.createDataChannel` in un
  init script: è l'oggetto reale che l'app sta usando, si sceglie solo il
  momento in cui il transport muore. (b) Il file di quel gate è 8 MiB + 7 B, non
  2 MiB: un quarto di 8 MiB sono DUE chunk interi, quindi "guasto al 25 %" lascia
  davvero qualcosa di verificato su disco da saltare. (c)
  `helpers.mjs` accetta `BORE_E2E_LOG` per inoltrare lo stderr del server: opt-in,
  una corsa verde resta silenziosa. (d) `T-WEB-DIRECT-TIMEOUT` usa
  `forceIceRelayOnly` e misura il COMPORTAMENTO (nessun commit `direct`, nessun
  byte letto prima del commit relay, una sola `transfer.request`), non il tempo:
  con policy `relay` e nessun TURN i motori falliscono ICE molto prima dei 10 s
  di deadline, che resta il backstop e non il cronometro.
- **Cosa eredita 4.4.** `transfer.progress` è già il canale con cui il server
  dice alla fonte su che path stanno viaggiando i byte, quindi `T-WEB-PATH-AUTH`
  ha già il suo osservabile; `applyCommitPlan` rende il piano una proprietà del
  commit, quindi uno scenario multipeer non può far divergere due capi; e
  `forceIceRelayOnly` è già in `fixtures.js`.

### 4.4 Validare direct-first e fallback nello scenario multippeer reale

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** costruire lo scenario A/B/C esatto con WebRTC reale e fallimento ICE imposto dal browser; correggere ogni difetto e self-review che il test non falsifichi il risultato applicativo.
- **Files:** `web/transfer/tests/e2e/multipeer.spec.mjs:new`; `web/transfer/tests/e2e/fixtures.js:RTC wrapper — policy and counters`; `tests/web_transfer_test.rs:path metrics group — server assertions`; `scripts/web_transfer_e2e.sh:browser orchestration — direct/fallback mode`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** avviare un'unica room e tre contesti browser. A pubblica un file canary. B clicca e usa WebRTC reale; verificare DataChannel aperto, payload esatto, `path=direct` solo dopo primo chunk e zero relay permits/socket. Per C installare prima di caricare la pagina un wrapper trasparente di `RTCPeerConnection` che preserva constructor/prototype/static behavior ma sostituisce sempre config con `iceTransportPolicy:"relay"`; poiché la configurazione non contiene TURN, ICE deve fallire naturalmente. C clicca una volta, attende la deadline e riceve lo stesso file via relay; verificare attempt diverso, path relay e un solo permit. Non impostare manualmente state, non chiamare direct_failed dal test e non bloccare l'intero UDP del server: il test deve attraversare signaling e timer reali. Poi B e C pubblicano propri file e A vede entrambi, provando simmetria. Avviare download A→B e B→C concorrenti e verificare che ogni fonte resti la sola sorgente della propria offerta; B non deve servire automaticamente il file ricevuto da A. Infine B annulla un proprio download e gli altri trasferimenti continuano. Acquisire evidenze tramite WebSocket paths, peer UI, server metrics crate-private e hash output, mai affidandosi a una sola log line.
- **Unit tests:** frontend `rtc_policy_wrapper_preserves_api_and_forces_relay_only`; server `direct_and_relay_attempt_counters_follow_recipient_report`; `recipient_report_cannot_change_unrelated_transfer_path`; `downloaded_file_does_not_create_offer_or_seed`.
- **e2e tests:** `T-WEB-MULTIPEER` — A pubblica; B scarica direct; C con ICE relay-only scarica via relay; B/C pubblicano; catalogo converge; B annulla; nessun auto-seed o auto-download; `T-WEB-PATH-AUTH` — path resta connecting fino al primo chunk verificato e segue il report recipient.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + T-WEB-MULTIPEER passa almeno 10 volte senza flake su Chromium e una volta nelle combinazioni Firefox/WebKit supportate + le evidenze distinguono effettivamente DataChannel e route relay + self-review conferma che il wrapper modifica soltanto la policy ICE + closed in `STATE.md` (§1 → 4.5, §4 ledger row, §6 `none`, §11 board).

**Esito (2026-09-16).** 4.4 implementato e chiuso. Lo scenario reale a tre peer
ha trovato **tre difetti di prodotto** — tutti sul percorso di fallback, tutti
invisibili ai test scritti prima di lui — e due verità sul tempo che hanno
cambiato non il prodotto ma quello che un gate può legittimamente pretendere.

- **`T-WEB-MULTIPEER`, una room e tre contesti con profilo su disco.** A
  pubblica; B usa WebRTC reale e legge `pathCommits ["direct"]`, una sola
  `RTCPeerConnection`, **zero** socket relay e l'hash esatto; C monta prima del
  caricamento il wrapper `applyRelayOnlyPolicy` (`iceTransportPolicy:"relay"`
  senza alcun TURN, quindi ICE fallisce da solo) e sullo STESSO click ottiene
  `["relay"]`, un attempt DIVERSO, esattamente una socket relay e una sola
  `transfer.request`. Poi B e C pubblicano, tutti e tre convergono su tre
  offerte con tre proprietari distinti, B tira `gamma` mentre C tira `beta`, B
  annulla la PROPRIA riga (`data-cancel` scoped sulla riga) e beta arriva
  intatta. L'evidenza non è mai una riga di log: percorso committato, conteggio
  `RTCPeerConnection`, URL delle socket, hash del file salvato e conteggi di
  `transfer.request` sono quattro fonti indipendenti.
- **`T-WEB-PATH-AUTH`, e il perché della sua room privata.** Il bucket del
  relay ha `burst = 2 x rate`: un file più piccolo del burst viene consegnato
  di colpo, quindi in una room a 1 MiB/s **non esiste** una finestra fra il
  commit e il primo byte verificato. Il test ha quindi la propria room a
  256 KiB/s con un file da 1 MiB+7. Il taglio è il PRIMO BYTE VERIFICATO DAL
  DESTINATARIO, non il commit, perché i motori non falliscono ICE allo stesso
  modo (Chromium esaurisce la deadline di 10 s, Firefox fallisce in meno di un
  secondo): prima di quel byte ogni campione legge `connecting` e mai
  `direct`, dopo segue il destinatario.
- **I contatori seguono i byte verificati, non il commit (F-12).**
  `direct_carried`/`relay_carried` contano un attempt **una volta sola**, al
  primo `transfer.progress` del destinatario con byte verificati su quel
  percorso. Un tentativo relay ammesso che non ha portato nulla NON è un
  percorso portato — è esattamente l'errore che `direct_stream_opens` fece
  salire 1 → 12 durante un blackout in cui non si muoveva niente.
- **Difetto 1 — il badge fermo su "in connessione" su OGNI relay.**
  `socket.onclose` dimenticava il trasferimento a chiusura ordinata dopo il
  FINAL, quindi il `transfer.progress` del destinatario — l'unica autorità che
  la sorgente ha sul percorso — cadeva nel vuoto. Correzione: la chiusura
  ordinata chiude la GAMBA, non il TRASFERIMENTO; il record resta finché il
  destinatario non ha riferito.
- **Difetto 2 — la sorgente troncava la propria gamba relay.** Chiudeva la
  socket con megabyte ancora in coda: `bufferedAmount == 0` significa "il
  motore ha preso il messaggio", non "il peer l'ha letto". MISURATO su WebKit:
  di 8 388 613 byte scritti ne arrivavano 7 087 168, il server leggeva lo
  stream come `SourceGone` e faceva fallire con `DIRECT_FAILED` un
  trasferimento **completo**, per entrambi i peer. Correzione in due parti: il
  drain dopo il FINAL non è più condizionato dall'harness, e soprattutto il
  **FINAL chiude la pompa lato server** — la sorgente non chiude più nulla, su
  nessuno dei due trasporti. Documentato in `WEB_TRANSFER_PROTOCOL.md` §3.
- **Difetto 3 — le range verificate perse quando la sorgente vinceva la
  corsa.** `abandonDirect` (la notifica del contropartente arrivata per prima)
  chiudeva la metà locale e "non mandava niente": il destinatario è l'unico a
  sapere cosa ha già verificato su disco, quindi quella informazione cessava di
  esistere e l'attempt di sostituzione rispediva tutto. Ora il destinatario
  riferisce anche da lì (il server adotta il report tardivo, 4.3
  `adopt_late_recipient_resume`), e l'esattamente-una-volta è garantito da
  `receiver.directFailed`, che risponde `null` per un attempt già riferito —
  red-check: togliendo la guardia arrivano due report.
- **Prima verità sul tempo: la RAGIONE del report è di chi se ne accorge
  prima.** Il `close` del destinatario dice `channel-closed`, la scrittura
  fallita della sorgente dice `send-error` e il destinatario, avvisato prima
  che il proprio evento scatti, riporta ciò che gli è stato detto. Sono due
  affermazioni vere sullo stesso canale morto e quale vinca lo decidono i timer
  di due motori: fissarne una significa testare lo scheduler. Il gate pretende
  ciò che il prodotto garantisce — **un solo** report, sull'attempt giusto, con
  le range dentro.
- **Seconda verità: i byte sulla socket non sono lavoro verificato.** Sotto
  carico WebKit teneva DUE chunk interi non ancora verificati quando il canale
  moriva: nulla di verificato, quindi nulla da saltare, e il relay rispediva
  tutto. È un caso di prodotto corretto (l'hash corre dietro al filo), ma non è
  il caso che questo gate esiste per dimostrare, e uccidere il canale lì
  faceva misurare al gate il ritardo della pipeline di hash. Ora
  `killChannelAfter` aspetta i byte **e** un chunk verificato, leggendolo da
  `receiverState()`. Nota per il futuro: NON si è scelto di verificare i chunk
  pendenti prima di riferire — ritardare il report aumenta esattamente la
  probabilità di perdere la finestra del difetto 3.
- **Rumore di motore, filtrato per nome e mai per silenzio.** Firefox scrive
  `ICE failed, add a TURN server` (è la condizione che il test IMPONE) e
  `The connection to ws://… was interrupted while the page was loading` quando
  si chiude un contesto: due pattern espliciti, con lo stesso precedente di
  T-WEB-REPUBLISH. Gli errori dell'applicazione dicono altro e non sono mai
  filtrati.
- **Soak.** `STAGES=soak scripts/web_transfer_e2e.sh` (nuovo stage, fuori dal
  default perché costa minuti) esegue `T-WEB-MULTIPEER` N volte su Chromium:
  **12/12 verdi**. `T-WEB-DIRECT-FALLBACK` è stato inoltre ripetuto 30 volte
  sotto carico sui tre motori insieme (8 worker) senza un fallimento.


### 4.5 Update README.md

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** aggiornare la promessa pubblica a direct-first con fallback automatico e fare self-review di esempi e troubleshooting.
- **Files:** `README.md:290 — command overview`; `README.md:881 — self-hosting`; `README.md:996 — HTTPS/networking`; `README.md:2035 — secure file transfer`; `README.md:3200 — troubleshooting`.
- **Change:** preservare la struttura, il tono e la lingua esistenti del README, modificandolo senza riscriverlo. Modificare la sezione web transfer affinché dichiari WebRTC DataChannel diretto come percorso predefinito e relay WebSocket cifrato come fallback automatico dello stesso download. Spiegare che non serve alcun flag, che il primo click è unico, che un guasto direct a metà riprende i chunk verificati via relay e che il path compare nella UI. Documentare STUN default/derivato, lista custom, `--web-transfer-no-stun`, assenza TURN, porte/firewall richiesti e comportamento con UDP/WebRTC bloccato. Mantenere esplicita la limitazione ancora reale a file singoli. Aggiornare esempi deploy binary e SSH gateway coerentemente, senza dettagli di SDP, classi o state machine.
- **Unit tests:** docs/help consistency per flag STUN e network examples.
- **e2e tests:** `T-WEB-README-DIRECT` — eseguire gli esempi README una volta con direct e una con ICE impedito, verificando stesso gesto utente e hash finale.
- **Done:** un nuovo utente comprende quando usa direct o relay e come configurare rete/STUN dal README; esempi eseguiti; tutti i gate verdi; self-review completa; closed in `STATE.md` con §1 → 5.1 e la §11 docs row per Phase 4 `DONE`.

**Esito (2026-09-16).** 4.5 implementato e chiuso. Il README era già
direct-first nella PROMESSA (4.2/4.3 avevano riscritto il blocco "What this
release does" e 4.4 ne ha provato il contenuto); quello che mancava era la
RETE, cioè tutto ciò che decide quale dei due percorsi un lettore otterrà.

- **Nuova sezione `Direct or relay, and what decides it`**, dopo il flusso in
  pagina e prima del troubleshooting: il diretto è il default e non ha flag;
  qualunque cosa lo impedisca produce **un solo** tentativo di rimpiazzo sul
  relay, sullo stesso click, conservando ciò che il destinatario ha già
  VERIFICATO; la riga dichiara il percorso che ha PORTATO i byte e fino ad
  allora legge `in connessione`; entrambi i percorsi trasportano gli stessi
  frame AES-256-GCM, quindi cadere sul relay cambia la rotta e non la
  garanzia.
- **STUN documentato come catena, non come concetto.** La lista di default è
  il responder STUN del server (solo se `bore server --udp` gliene dà uno)
  seguito dalla catena pubblica che il resto di bore già usa, citata per nome;
  `--web-transfer-stun` la SOSTITUISCE, `--web-transfer-no-stun` lascia i soli
  candidati host — giusto su una LAN chiusa e dentro una misura, ma su
  internet manda quasi ogni coppia sul relay. I due flag si contraddicono e il
  server li rifiuta all'avvio.
- **Perché non c'è TURN, scritto una volta e per sempre.** Un TURN è un relay
  da far girare, accreditare e fidare; bore ne ha già uno — il proprio, sulla
  porta da cui la room è già servita, che trasporta cifrato che non può
  leggere. La coppia che TURN salverebbe è esattamente quella che il fallback
  serve già.
- **Porte e firewall.** Il server non apre nulla oltre alla porta di controllo
  e il percorso diretto non ne lega alcuna su di lui; serve **UDP in uscita da
  entrambi i browser** e nessuna porta in ingresso da nessuna parte. Aggiunto
  anche al puntatore di self-hosting, alla ricetta di deploy 12 (che ora dice
  cosa fare su una rete chiusa: `--web-transfer-no-stun`) e al paragrafo del
  demux, perché `--ssh-gateway` e `--web-transfer-base-url` convivono su una
  sola porta senza configurazione aggiuntiva.
- **Tre righe nuove di troubleshooting**, tutte su sintomi che un utente vede
  davvero: tutto `relay` anche in LAN (no-stun, o UDP in uscita bloccato da un
  lato), una riga passata da `diretto` a `relay` a metà (atteso: i chunk
  verificati sono stati tenuti, il file salvato è identico) e una riga ferma
  su `in connessione` (il badge segue i byte verificati, non le intenzioni).
- **Gate `t_web_readme` esteso a ciò che può derivare.** Le tre promesse di
  policy sono asserite come testo, ma la catena STUN di default è una
  COSTANTE del prodotto (`holepunch::PUBLIC_STUN`): la prosa che cita una
  costante ci si scolla in silenzio, quindi il test la confronta server per
  server. Red-check fatto: cambiando `stun1` in `stun9` nel README il gate
  fallisce nominando il server mancante.
- **Gate `T-WEB-README-DIRECT` (nuovo, `readme-direct.spec.mjs`).** Avvia la
  room con le DUE righe di comando documentate e **senza alcun flag STUN**
  (quindi prova che il default funziona senza raggiungere un servizio
  pubblico), poi fa lo STESSO gesto documentato due volte: un lettore che può
  aprire un DataChannel e uno il cui ICE non può accoppiarsi. Stesso click,
  stesso hash, badge diversi, una sola `transfer.request` per lettore, zero
  socket relay per il diretto ed esattamente una per l'altro, e la sorgente
  che non fa niente di diverso per i due. Verde sui tre motori; red-check
  fatto forzando `iceRelayOnly` anche sul lettore diretto — il gate legge
  `relay` dove pretende `direct`.


### 4.6 Portare sul percorso browser le ottimizzazioni misurate del resto di bore (aggiunta in esecuzione, 2026-09-15)

> Sub-fase aggiunta su richiesta esplicita dell'utente (D22): *"bore spinge tanto su UDP e
> su QUIC… cura particolarmente la parte direct peer to peer"* e *"porta in campo le
> ottimizzazioni, ove possibile, che sono state fatte nel resto dell'applicazione"*.
> Eseguita DOPO 4.3 (il transport esiste) e PRIMA di 4.4, così lo scenario multipeer
> misura già il percorso ottimizzato.

- **Model:** `agent-1:Claude-Opus-5`
- **Assignment:** tradurre, una per una, le ottimizzazioni che il resto di bore ha MISURATO sul proprio data path nell'equivalente browser, misurando ciascuna con l'harness 3.9; rifiutare esplicitamente quelle che non si trasferiscono, con la ragione.
- **Files:** `web/transfer/src/webrtc.js:AttemptRtc — backpressure e sizing`; `web/transfer/src/sender.js:EncryptedFrameSink — batching e zero-copy`; `web/transfer/src/receiver.js:EncryptedFrameSource — decrypt in place`; `web/transfer/src/framing.js:encode/decode — allocazioni`; `web/transfer/src/storage.js:staging — scritture`; `web/transfer/tests/perf/throughput.perf.mjs:braccio direct`; `docs/transfer/WEB_TRANSFER_PERF.md:tabella direct`; `docs/transfer/WEB_TRANSFER_PROTOCOL.md:nota frammentazione`.
- **Change:** il catalogo è chiuso e ogni voce è "adottata perché misurata" oppure "rifiutata perché".
  1. **Backpressure, mai coda profonda (BW-F3 + V-13).** Il mittente ATTENDE `bufferedamountlow` invece di accodare: una coda profonda compra throughput e paga latenza, e nel nostro caso la coda vive nella heap del tab. High/low water restano 4 MiB/1 MiB (4.2) e la modifica è misurarli, non alzarli per riflesso.
  2. **Frammento dimensionato sul peer, non su una costante (RFC 8841 / I-M3).** `min(24576, maxMessageSize - 64)` è già in 4.2; l'aggiunta è misurare 8/16/24 KiB sul braccio direct e registrare il vincitore per motore, perché il costo per messaggio SCTP non è lo stesso ovunque.
  3. **Una connessione per trasferimento, nessuno striping per messaggio (BW-F2).** Il piano vieta già di condividere una `RTCPeerConnection`; qui si vieta esplicitamente anche il rovescio — N DataChannel su cui distribuire i frammenti dello STESSO file. Su un canale ordinato è inutile, su canali diversi riordina: è la stessa trappola per cui il percorso diretto nativo è flow-pinned e non round-robin.
  4. **Una allocazione e una copia per frame (V-14b).** `framing.js` compone l'header e il ciphertext in UN buffer e `crypto.js` decifra in place dove la WebCrypto lo consente; il test di non-regressione è il vettore fixture, byte per byte, perché il formato NON si muove.
  5. **Coalescing solo dove non diventa attesa (V-14a).** Frammenti già pronti si spediscono nello stesso turno di event loop; non si introduce MAI un timer che aspetti compagnia.
  6. **Rifiutate, con la ragione:** niente `SO_*BUF`/window tuning (il browser non espone la socket UDP: il tuning equivalente è la coda SCTP del punto 1); niente riuso di QUIC/holepunch nativo o di una seconda socket UDP (D5, invariante di piano); niente ritrasmissione applicativa (SCTP affidabile ordinato la fa già, e un livello in più è il TCP-over-TCP che V-15 descrive); niente `--carriers` browser (punto 3).
- **Unit tests:** `sender_waits_for_drain_instead_of_queueing_past_high_water`; `fragment_size_is_derived_from_peer_max_message_size`; `one_channel_per_transfer_and_extra_channels_are_refused`; `frame_encode_allocates_once_and_matches_the_fixture_bytes`; `coalescing_never_waits_on_a_timer`.
- **e2e tests:** `T-WEB-PERF-DIRECT` — l'harness 3.9 aggiunge il braccio direct e pubblica `direct / relay` come RAPPORTO misurato nella stessa ripetizione; fallisce rumorosamente se il braccio direct non produce byte (un direct che tace è il difetto, non un numero mancante).
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-PERF-DIRECT` produce la tabella con il rapporto direct/relay + ogni voce del catalogo è adottata con misura o rifiutata con motivo scritto in `docs/transfer/WEB_TRANSFER_PERF.md` + closed in `STATE.md` (§1 → 4.4, §4 ledger row, §6 `none`, §11 board).

**Esito (2026-09-16).** 4.6 implementato e chiuso. Il catalogo è chiuso: cinque
voci adottate (una sola con un guadagno misurato), quattro rifiutate con la
ragione, e due ipotesi FALSIFICATE su un difetto di transport che resta aperto
e documentato. Tabelle, campioni grezzi e comandi in
`docs/transfer/WEB_TRANSFER_PERF.md`.

- **Un'etichetta che aveva smesso di essere vera.** Da 4.2 il percorso diretto
  è il DEFAULT, quindi un destinatario che non chiede nulla misura WebRTC: le
  righe pubblicate in 3.11 dicevano `relay` e misuravano il DataChannel. Il
  confronto di 3.11 resta valido (due bracci a una variabile, stesso
  transport), ma l'intestazione no. Ora ogni braccio DICHIARA il proprio
  transport (`noWebRtc`) e il confronto staging è fissato sul relay, così
  continua la linea di 3.9/3.10.
- **`T-WEB-PERF-DIRECT`.** Nuovo test `direct versus relay throughput`: due
  destinatari a una variabile (WebRTC presente / motore senza WebRTC), ordine
  alternato, entrambi DENTRO la stessa ripetizione, e il rapporto calcolato
  **per ripetizione** e solo dopo mediano — dividere due mediane prese a
  minuti di distanza nasconderebbe esattamente la deriva che l'alternanza
  serve a cancellare. Fallisce rumorosamente se il braccio diretto non produce
  byte, e verifica sulla pagina di OGNI destinatario che i `path_commit` siano
  tutti `direct` o tutti `relay` (la SORGENTE serve entrambi i bracci: la sua
  lista ne contiene legittimamente uno per tipo, e asserire su di essa è stato
  il primo errore del test).
- **Il risultato, e non è quello che ci si aspetta.** Su chromium in loopback
  il braccio diretto è **BIMODALE**: una ripetizione sana legge 42-82 MiB/s,
  circa una su due o tre crolla a 2-14 MiB/s con `src.drain` fra 1,4 e 3,3 s.
  Il relay è piatto a 81-94 MiB/s con `src.drain` 0,0 ms. Rapporto mediano
  0,697x a 8 MiB. **Due ipotesi testate e falsificate:** la profondità della
  coda (sweep 4 MiB/1 MiB, 1 MiB/256 KiB, 256 KiB/64 KiB — il crollo c'è a
  ogni profondità) e la catena STUN pubblica (ri-misurato con
  `--web-transfer-no-stun`, solo candidati host — 3 crolli su 6). Resta il
  transport: SCTP ordinato e affidabile su UDP che perde un pacchetto e paga
  un RTO. È la forma browser di P-13, ed è l'unica manopola che una pagina non
  può raggiungere — il motivo per cui la voce 6 del catalogo RIFIUTA il tuning
  della socket invece di tentarlo.
- **E il rapporto non è un'affermazione sul prodotto.** Qui entrambi i
  "percorsi" sono loopback: il braccio relay è un salto TCP su localhost verso
  un server Rust sulla stessa macchina, cioè la condizione più favorevole che
  un relay possa avere, mentre il diretto paga comunque DTLS e SCTP per un
  peer a un processo di distanza. Vale V-9 senza modifiche: **qualificare il
  link prima di citare un valore assoluto**, e questo link non è quello per
  cui il percorso diretto esiste. Solo una misura a due host può dire chi
  vince in esercizio, ed è l'ambiente di 4.4, non di 4.6.
- **La sola ottimizzazione con un guadagno misurato: una allocazione e una
  copia per frame (V-14b).** `openWithKey` faceva due `slice` del messaggio in
  arrivo — header e body — allocando e copiando l'INTERO frame due volte in
  più, ~1400 volte per 32 MiB. WebCrypto accetta un `BufferSource`, quindi
  header e body sono ora `subarray`: `dst.open` scende da 42,4 a 28,9 ms a
  8 MiB (-31,8 %) e da 141,5 a 92,8 ms a 32 MiB (-34,4 %), con `dst.busy` come
  controllo immobile (351,9 → 352,5 ms). End to end vale ~6 % a 8 MiB e nulla
  di misurabile a 32 MiB, perché l'apertura si sovrappone alla ricezione. Il
  lato SEAL resta com'è ed è una decisione: `subtle.encrypt` restituisce
  sempre un `ArrayBuffer` nuovo e non esiste variante in place, quindi una
  allocazione e una copia del ciphertext sono il pavimento;
  `new Uint8Array(arrayBuffer)` avvolge e non copia, quindi ci siamo già.
- **Le altre quattro voci: adottate senza cambiare nulla, ma ora misurate e
  vincolate.** Backpressure 4 MiB/1 MiB confermata (confrontando solo le
  ripetizioni sane, 4 MiB e 1 MiB sono indistinguibili e 256 KiB è **25 %
  peggio**: una coda così bassa affama l'associazione fra due riempimenti);
  frammento derivato dal peer con 24 KiB vincitore misurato (8 e 16 KiB sono
  entrambi peggio, e solo il braccio a 24 KiB ha prodotto tre ripetizioni
  senza crolli); un canale per trasferimento, con il divieto esplicito anche
  del rovescio (N canali per lo STESSO file lo riordinano, perché SCTP ordina
  per stream — la trappola di BW-F2); coalescing che non diventa mai attesa.
- **Rifiutate, con la ragione scritta:** tuning di socket/window (una pagina
  non raggiunge la socket UDP sotto una `RTCPeerConnection` e non esiste API
  per il buffer SCTP — l'equivalente è la coda della voce 1, che è stata
  swept); riuso di QUIC/holepunch nativo o una seconda socket UDP (D5,
  invariante di piano); ritrasmissione applicativa (SCTP è già affidabile e
  ordinato: un secondo livello sopra è il TCP-over-TCP di V-15); `--carriers`
  browser (voce 3); rami per motore (stessa ragione per cui 3.11 ha rifiutato
  un ramo di storage).
- **Manopole solo per l'harness, inerti senza il sink.** `perf.js` espone
  `perfFragmentBytes` e `perfWaterMarks`; `webrtc.js` le legge UNA volta per
  attempt (una soglia che si muovesse sotto un canale vivo farebbe divergere
  la pipeline dal `bufferedamountlow` del motore) e il numero del peer resta
  il SOFFITTO: l'harness può solo rimpicciolire il frammento, mai alzarlo
  oltre ciò che il peer accetta. `perfWaterMarks` richiede entrambe le soglie
  e `low < high`, altrimenti valgono quelle di serie — una manopola malformata
  non deve mai diventare una coda di zero. `spawnRoomEnv` accetta `noStun`.
- **Gate.** Unit JS 123/123 (+5: i tre di `optimizations.test.mjs` e i due
  aggiunti a `webrtc.test.mjs`). `scripts/perf/web_transfer_bench.sh` esclude
  gli sweep (`--grep-invert sweep`) e continua a fallire rumorosamente su
  `median=FAILED`, che ora copre anche la riga del rapporto.
- **Red-check** (fatti e ritirati): togliere `await waitForLowWater` fa cadere
  `sender_waits_for_drain_instead_of_queueing_past_high_water`; aggiungere un
  solo `setTimeout(…, 0)` nel loop di invio fa cadere
  `coalescing_never_waits_on_a_timer`.
- **Cosa resta aperto, e 4.4 NON lo chiude.** Lo scenario multipeer sono tre
  contesti browser sulla STESSA macchina: è lo stesso loopback, quindi non può
  rispondere. La domanda richiede due host reali con un percorso reale in
  mezzo — un ambiente che questo piano non ha mai avuto. Registrato in
  `STATE.md` §9; finché non esiste, il rapporto resta una proprietà della
  misura e non del prodotto, e il README dice che il badge riporta quale
  percorso ha portato i byte, non quale era più veloce.

---

## Phase gates

- **Build:** `cargo build --all-features`
- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** `cargo test --all-features --lib && cargo test --all-features --test web_transfer_test -- --test-threads=1 && npm ci --prefix web/transfer && npm run check --prefix web/transfer && npm run test:e2e --prefix web/transfer`
- **Asset drift:** `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist`
- **Regression guard:** `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ && cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`; T-WEB-NOAUTO, T-WEB-DIRECT, T-WEB-DIRECT-FALLBACK, T-WEB-PERF-DIRECT, T-WEB-MULTIPEER e tutti i relay/security gate Phase 3 passano.
- **README:** aggiornato per direct-first/fallback, STUN e requisiti rete; la limitazione file singolo resta chiara.

## Phase done criterion

B scarica da A direttamente con WebRTC dopo un solo click; C, con ICE realmente incapace di trovare una coppia, passa automaticamente al relay; un guasto direct a metà conserva i chunk verificati e completa sul relay con nuova chiave. Il server non riceve payload plaintext e non apre nuova UDP. README.md descrive il comportamento direct-first e `STATE.md` §11 mostra Phase 4 `DONE` con ogni subfase chiusa.
