# Campagna prestazioni e difetti — 10‑13 settembre 2026

Questa cartella contiene **tutto** ciò che la campagna ha prodotto: il piano, i
requisiti del banco, le evidenze grezze di ogni blocco di misure, le revisioni
per componente e — per ogni componente — un file **`FINALE.md`** in italiano con
tre sezioni sole: *cosa è risultato*, *cosa è stato cambiato*, *cosa resta
aperto*.

Se hai poco tempo, leggi in quest'ordine:

1. [`RELAZIONE_FINALE_CABLATO_2026-09-13.md`](RELAZIONE_FINALE_CABLATO_2026-09-13.md) — la relazione generale.
2. Il `FINALE.md` del componente che ti interessa (tabella sotto).
3. Le evidenze, solo se vuoi il numero grezzo dietro un'affermazione.

## La finestra e la linea

La campagna è nata su WiFi e si è **chiusa via cavo**, e questa non è una nota
di colore: è la ragione per cui metà delle conclusioni assolute della prima
parte sono state riscritte. La linea cablata di questa workstation, misurata
verso tre destinazioni indipendenti, è **~925 Mbit/s in download e ~740 in
upload** a P=8; la stessa linea via radio dava 369‑416 in download. Ogni
percentuale «di banda nuda» in questo repository si riferisce alla linea
cablata.

Due regole che ne discendono e che valgono per chiunque rifaccia queste misure:

- **Un rapporto contro un controllo nudo campionato nella stessa ripetizione è
  valido su qualunque linea; un valore assoluto in Mbit/s no.** (V‑9)
- **Un trasferimento più corto della rampa misura la rampa.** I byte per braccio
  sono tarati contro una linea: quando la linea cambia, ogni conteggio diventa
  in silenzio un esperimento diverso. Per questo la dimensione è una variabile
  (`XFER_MB`) e ri‑derivarla è la prima cosa che si fa dopo un cambio di
  collegamento. (V‑19)

## I componenti

| cartella | componente | il risultato in una riga |
|---|---|---|
| [`vpn/`](vpn/FINALE.md) | VPN L3 (1:1 e hub) | il percorso diretto vale il **94 %** della linea in upload; il «deficit del 37 %» era della radio |
| [`vhost/`](vhost/FINALE.md) | reverse proxy per sottodominio | **95 %** della linea; relay e diretto si separano sul **costo**, non sulla velocità |
| [`public/`](public/FINALE.md) | tunnel pubblici `bore local` | trovato P‑14: un tunnel `--udp` ri‑registrato perdeva il percorso diretto **in silenzio** |
| [`secret/`](secret/FINALE.md) | tunnel segreti punto‑punto | **92 %** della linea; il diretto costa meno in totale e scarica il server |
| [`transfer/`](transfer/FINALE.md) | `bore transfer` | **99 %** della linea, e **2,94×** più veloce su 20 000 file dopo le correzioni |
| [`ssh-jump/`](ssh-jump/FINALE.md) | jump host SSH | **l'82 % dell'apertura di sessione era un `sleep`**: 1,25 s → 0,66 s |
| [`server/`](server/FINALE.md) | server, registri, admin API | sei difetti che a `info` non si vedevano: il tunnel sembrava sano e non serviva |
| [`nat-udp/`](nat-udp/FINALE.md) | traversal UDP e diagnostica | la diagnostica ora attraversa **come attraversa il prodotto**; trovato W‑1 |

## Gli altri documenti

| file | cos'è |
|---|---|
| [`RELAZIONE_FINALE_CABLATO_2026-09-13.md`](RELAZIONE_FINALE_CABLATO_2026-09-13.md) | relazione generale: difetti di prodotto, difetti di strumento, compilatori del lint, domande aperte |
| [`ETH_CAMPAIGN_PLAN.md`](ETH_CAMPAIGN_PLAN.md) | il piano della finestra cablata: cosa si rimisura, cosa **no** e perché |
| [`STAGING_REAL_BENCH_REQUIREMENTS_2026-09-10.md`](STAGING_REAL_BENCH_REQUIREMENTS_2026-09-10.md) | requisiti del banco di prova, lato applicativo |
| [`evidenze/`](evidenze/) | le evidenze grezze, un documento per blocco di misure |

Le evidenze sono in gran parte in inglese (sono state scritte accanto agli
strumenti, che sono in inglese); i `FINALE.md` e le revisioni `final_*` sono in
italiano.

## Come rifare i test

L'harness è in [`scripts/perf/staging/`](../../scripts/perf/staging/); il suo
[`README.md`](../../scripts/perf/staging/README.md) elenca ogni fase, le
coordinate che legge dall'ambiente (**mai dal repository**) e le **54 trappole**
che questa campagna ha incontrato, con il compilatore di `lint.sh` che oggi
rifiuta ciascuna a macchina. Non serve ricominciare da capo: serve rileggere la
linea (`asym_qualify.sh`), ri‑derivare `XFER_MB`, e rieseguire la fase.
