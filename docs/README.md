# Indice della documentazione

Una cartella per componente. Alla radice di `docs/` non sta nessun documento:
solo questo indice.

## Componenti

| cartella | cos'è |
|---|---|
| [`vpn/`](vpn/) | la VPN L3: piani, port su macOS/Windows/Android, hardening, NAT 1:1, hub multi‑client, guida utente |
| [`vhost/`](vhost/) | il reverse proxy per sottodominio: piano, audit, TLS verso il backend, il fix del flush iniettato, la concorrenza `--udp` |
| [`ssh-gateway/`](ssh-gateway/) | l'ingresso SSH (`--ssh-gateway`): guida operativa, jump host, demux ALPN, audit e bug hunt |
| [`transfer/`](transfer/) | `bore transfer`: [guida italiana completa a `bore transfer link`](transfer/TRANSFER_LINK.md), fattibilità, piano di upgrade, audit, matrice di test |
| [`secret/`](secret/) | i tunnel segreti punto‑punto: hardening |
| [`local/`](local/) | `bore local` / `bore proxy`: assessment e il piano UDP per i tunnel pubblici |
| [`nat/`](nat/) | traversal UDP: riferimento del traversal, NAT adattivo, candidati manuali, confronto con lo stato dell'arte, limitazioni di firewall |
| [`server/`](server/) | ottimizzazione UDP lato server |
| [`weblog/`](weblog/) | logging di accesso in stile nginx |
| [`frontend/`](frontend/) | la dashboard `/admin/status`: architettura, piani, assessment |

## Trasversali

| cartella | cos'è |
|---|---|
| [`campagna-2026-09-13/`](campagna-2026-09-13/README.md) | **la campagna prestazioni e difetti del 10‑13 settembre 2026**: un `FINALE.md` per componente (risultato, ottimizzazioni, cosa resta aperto) più tutte le evidenze |
| [`plans/`](plans/) | i piani di lavoro per fasi, uno per progetto |
| [`platform/`](platform/) | supporto per sistema operativo: Android, requisiti di distribuzione macOS |
| [`install/`](install/) | installazione, URL di download, esecuzione «live» senza installare |
| [`comparisons/`](comparisons/) | confronti con altri progetti: frp, DERP di Tailscale |
| [`test/`](test/) | registro di copertura dei test e `bore test-udp` |
| [`project/`](project/) | stato del fork rispetto a upstream |
| [`performance/`](performance/) | ciò che precede la campagna di settembre (la campagna sta in `campagna-2026-09-13/`) |

## Dove sta la verità

`README.md` alla radice del repository è la **fonte unica** per l'uso: ogni
funzionalità, flag, flusso e passo di deploy sta lì. `CLAUDE.md` è l'elenco
degli **invarianti da non rompere**, uno per difetto trovato, con la misura che
lo ha provato. I documenti in questa cartella spiegano *perché* le cose stanno
così; il README dice *come si usano*.

## Regola

Un documento nuovo va nella cartella del suo componente. Se il componente non ha
una cartella, si crea — non si lascia il file alla radice. `lint.sh`
(`scripts/perf/staging/lint.sh`) rifiuta un link relativo che non si risolve, in
tutto `docs/`: se si sposta un documento, i riferimenti si spostano con lui.
