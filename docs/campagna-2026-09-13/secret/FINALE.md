# Tunnel segreti (punto‑punto) — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

## 1. Risultato

**Il percorso segreto raggiunge il 92 % della linea nuda**, e il vantaggio del
22 % che il diretto sembrava avere sul relay **era della radio**: via cavo i due
trasporti sono pari a connessione singola. Quello che il diretto compra davvero
è un'altra cosa: **costa meno in totale e scarica il server**, la cui CPU passa
da **49 core‑secondi a meno di 1**. Il percorso segreto costa **~0,2 ms** e
tiene il **94 % della linea**, e lo tiene anche sotto concorrenza (§17).

**Il difetto più insidioso era un'osservabilità che mentiva.** Il consumatore è
l'**unico** a sapere come è viaggiato il tunnel — il percorso diretto corre
consumatore↔provider e il server non ci sta sopra — e il suo rapporto veniva
mandato **una volta sola**, dopo la negoziazione di registrazione. Un
consumatore che parte prima del suo provider si sente rispondere
`UdpUnavailable`, si registra sul relay e **si aggiorna da solo** pochi secondi
dopo. Misurato sulla workstation: il consumatore scriveva `path=direct-udp` per
**200 connessioni consecutive** mentre l'admin API rispondeva `relay - no
udp-capable provider registered`, per il resto della sessione.

**Il round di controllo del listener era bimodale**, e la causa non era la rete:
`direct_ready_ms` dava 18 esecuzioni fra 37 e 53 ms e 9 fra 1036 e 1162, senza
niente in mezzo. Il ~1 s costante è il **PTO iniziale di quinn**
(`333 + 4 × 166 ms`). Ogni esecuzione lenta accoppia un dialer che nomina in
~210 ms con un listener che finisce a secco dopo 1126.

## 2. Ottimizzazioni apportate

| # | cosa | effetto misurato |
|---|---|---|
| **S‑2** | il rapporto sul percorso **segue il percorso**: l'aggiornamento relay→diretto manda un rapporto fresco, e la capacità di riportarlo viene **imparata** da qualunque `UdpPunch`, monotonicamente | un consumatore a cui era stato detto `UdpUnavailable` alla registrazione non vedeva mai quel messaggio, quindi non poteva riportare l'aggiornamento che stava per completare. La scrittura è **limitata nel tempo** perché vive in un braccio di `select!`: è la forma esatta di P‑9 |
| **S‑5** | il round del listener **finisce nell'istante in cui il dialer si autentica** | un peer che ha la chiave, è sulla generazione giusta, gioca il ruolo opposto e ci raggiunge da `src` ha già dimostrato tutto ciò che la metà del listener può dimostrare. La risposta va **sul filo prima** che il driver sia avvisato: chiudere il round ferma l'attore, quindi annunciare prima si mangerebbe il datagramma che il dialer aspetta |
| **S‑5 corollario** | un listener **non** esce dal round sulla propria `validated_rx` | quella validazione non dà niente al dialer, e uscire lì disabilita il round proprio mentre la richiesta del dialer — sull'ordine ordinario, il datagramma **successivo** — sta arrivando. Il costo non è un tunnel perso ma un **percorso veloce** perso, e un round a secco è anche ciò che **arma** la fuga a spruzzo di Fase 7 |
| **S‑7** | `DIRECT_INITIAL_RTT` 100 ms (`BORE_DIRECT_QUIC_INITIAL_RTT_MS`, `[10,333]`) al posto del default «nessuna informazione» di RFC 9002 | un endpoint diretto di bore esiste solo **dopo** uno scambio autenticato con quel peer o una connessione TCP di controllo a quell'host: sottostimare costa **un** Initial duplicato, sovrastimare costa un PTO intero di silenzio sul pacchetto che si perde più spesso |
| **S‑8** | `UDP_UPGRADE_MAX_SECS` da **256 s a 60** | era il tempo **peggiore** di permanenza sul relay dopo che il diretto era tornato possibile: quattro minuti e mezzo di relay per una rete sana da quattro. Due `const _: () = assert!` a tempo di compilazione lo tengono lì: un test unitario **riporta** la regressione dopo la build, un assert costante **rifiuta di produrla** |
| **S‑9** | la cache della coppia vincente è **limitata** (`PAIR_CACHE_MAX` 256) e la scadenza viene spazzata da `remember` | la scadenza girava solo dentro `recall` e solo per la chiave richiesta: una chiave mai più richiesta non veniva più esaminata, e un processo che contatta molti tunnel teneva una voce per id **per tutta la vita** |
| **BUG‑S1/S2** | i carrier del consumatore non registrano una riga admin e non vengono mietuti | `--carriers N` mostrava N‑1 righe fantasma «N/A», e i carrier (che per progetto non battono) venivano mietuti a 60 s degradando il pool da N a 1 |

## 3. Rimasto aperto

**D7 — un consumatore dietro CGNAT si connette legittimamente da una sorgente
non offerta** (`100.64/10`). Per questo il filtro sulla sorgente accettata è
stato **rifiutato** e la migrazione QUIC **non** viene disabilitata:
l'autenticazione con token è il cancello, non l'indirizzo. Non è un difetto
aperto ma una decisione da non rimettere in discussione senza nuove prove.

**Le strade benigne dell'hole punch restano a `debug`, mai `WARN`.** Stessa
regola, stesso motivo: sono l'attraversamento normale, non un guasto.

## 4. I documenti di questo componente

| file | cos'è |
|---|---|
| [`final_secret_perf_review.md`](final_secret_perf_review.md) | la revisione completa in italiano |
| [`../evidenze/SECRET_STAGING_EVIDENCE_2026-09-11.md`](../evidenze/SECRET_STAGING_EVIDENCE_2026-09-11.md) | la campagna punto‑punto |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | §16‑18 |
| [`../nat-udp/FINALE.md`](../nat-udp/FINALE.md) | il traversal e la diagnostica, che questo componente condivide |
