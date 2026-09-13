# Traversal UDP e diagnostica — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

## 1. Risultato

**La diagnostica non attraversava come attraversa il prodotto**, ed è l'errore
più costoso che uno strumento di diagnosi possa fare. `bore test-udp
--tcp-secret-id` è ciò che un operatore usa quando un tunnel vero non va in
diretto: un verdetto vale solo se viene dallo stesso percorso di codice. Non era
così. I percorsi reali (segreto e VPN 1:1) eseguono il **round autenticato** in
gruppi ordinati attraverso `listener_checks_then_quic` / `dialer_checks_then_quic`;
la diagnostica contattava tutti i candidati in concorrenza sul **vecchio punch
cieco** e stampava «Candidate order: advisory only». Su una coppia in cui il
round vince e il punch cieco perde — esattamente la cella `eim:adf × edm` che la
matrice misura — la diagnostica rispondeva **RELAY per una coppia il cui tunnel
va DIRETTO**. È la risposta sbagliata più cara che questo strumento possa dare.

**E il nuovo cancello ha trovato W‑1 appena acceso.** Due peer con lo **stesso**
binario più recente si sono dichiarati a vicenda «precedente al round
autenticato» e sono ricaduti entrambi sul punch cieco legacy — attraverso un
server la cui struct `UdpTestPeerSummary` non ha affatto il campo `checks`
(verificato con `git show`).

**W‑1, in generale**: un campo additivo `#[serde(default)]` è sicuro su un filo
**punto‑punto** e **non** lo è attraverso una parte che **ri‑serializza** il
messaggio. La regola di compatibilità di questo progetto poggia sul fatto che il
peer sia l'unica altra parte: uno vecchio ignora ciò che non conosce, uno nuovo
fornisce il default. Il percorso di pairing di `test-udp` rompe quella premessa:
il server tiene il riassunto come struct **tipizzata** e lo ri‑serializza verso
l'altro peer, quindi un server la cui definizione non ha un campo **non può
inoltrarlo**. Su quel canale «additivo» significa «cancellato dal middlebox», e
il `#[serde(default)]` che protegge la compatibilità diventa il valore che
**spegne la funzione**. Il guasto è **silenzioso** e degrada verso un percorso
più lento ma funzionante: è per questo che è sopravvissuto finché una
diagnostica non ha stampato, in una sola esecuzione, come i due lati vedevano
l'uno l'altro.

**Fase 6‑7, la cella che restava in RELAY.** Con esattamente **un** lato
simmetrico, l'asse che decide la cella è il **filtering del lato NON simmetrico**,
non la simmetria dell'altro — misurato: `eim:apdf × edm` → RELAY, mentre
`eim:adf × edm` e `eim:eif × edm` → DIRETTO tenendo il lato simmetrico costante.
Il pacchetto `simmetrico → altro` arriva da una porta imprevedibile, quindi il
filtro dell'**altro** deve essere più lasco di APDF; il pacchetto
`altro → simmetrico` arriva da una mappatura EIM stabile su cui il simmetrico ha
già scritto.

## 2. Ottimizzazioni apportate

| # | cosa | effetto |
|---|---|---|
| **V‑2** | `establish_direct` prende `Option<&CheckConfig>` ed **entra nelle stesse funzioni** del prodotto; il ramo `None` resta il legacy **identico byte per byte** | il cancello legge la capacità **del peer** da `UdpTestPeerSummary.checks`, mai una versione: un round che nessuno risponde è indistinguibile da una rete che ha mangiato i frame. Due dettagli sono **politica**, non incidente: la cache della coppia vincente **non** viene consultata (una diagnostica serve a misurare una coppia fredda) e il ruolo di spray viene preso **solo** da un piano brokerato dal server, perché due `plan_for_pair` calcolati in locale non sono garantiti complementari e due lati «easy» spruzzerebbero l'uno contro l'altro |
| **W‑1** | i due messaggi diagnostici nominano **entrambe** le cause — il peer è vecchio **oppure** il server ha cancellato il campo — e rimandano alla riga `Traversal round`, che porta una lettura indipendente sull'annata del server | una diagnostica non deve mai attribuire una capacità mancante al solo peer, perché il server è ugualmente in grado di averla persa. Il ramo `enforced` resta intatto: è quello che il cancello netns `T-NAT-DIAG-ROUND` verifica |
| **Fase 7** | la fuga a spruzzo per l'unica cella RELAY: il broker (unica parte che vede entrambi i profili) assegna i ruoli `easy`/`hard`, **complementari per costruzione**; il lato easy spruzza dalla propria socket del round (la sua **stabilità è l'asset**), il lato hard apre socket ausiliarie | con un sorteggio generoso le collisioni multiple sono la norma, quindi «vince il primo frame» fa scegliere **coppie diverse** ai due lati — trovato dal cancello su loopback, non ragionato. Il lato easy decide da solo e annuncia con richieste che portano un **transaction id già visto dal peer**: uno spray porta sempre un id fresco, quindi «un id che ho già visto» è un discriminante che lo spray non può contraffare |
| **Fase 7 (robustezza)** | un errore di `recv` su una socket di spray è **transitorio**, mai fatale | lo spray manda a centinaia di porte di cui al più una è aperta, quindi quasi ogni pacchetto si guadagna un ICMP port‑unreachable, che Windows consegna sulla socket **mittente** al `recv_from` successivo come `WSAECONNRESET`. Uscire su quello **scarta il datagramma che dimostra che la fuga ha funzionato**. Misurato: il test passava su Linux e falliva su `windows-latest`. Non riproducibile su Linux (una socket UDP **non connessa** non viene mai informata degli errori ICMP), quindi il job `windows-latest` è l'unico oracolo del modulo per questo |
| **ordine delle sonde** | la seconda osservazione con indirizzo alternativo di `discover_reflexive_profile` gira **dopo** `probe_filtering`, mai prima | **scrive** verso la coppia `(ip, porta)` alternativa che la sonda di filtering misura: invertite, un normale router in masquerade riportava `adf-or-eif` invece di `apdf` — la misura descriveva l'impronta della sonda stessa |

## 3. Rimasto aperto

**§54.1 — perché il leg VM → server resta a MTU 1200 su QUIC.** L'aritmetica è
chiusa (1202,7 B di payload UDP misurati contro i 1200 di `INITIAL_MTU` di
quinn, 0,2 % di errore), ma la sonda appaiata `test-udp` sullo **stesso** binario
misura **1452 con 4 sonde e 0 perse**: la scoperta del PMTU funziona. La domanda
si è **ristretta**, non chiusa. Vale ~17 % dei pacchetti del percorso diretto, e
l'istanza conta pacchetti (`pps_allowance_exceeded` 109 contro 18 714).

**Spostare la capacità del round fuori da un canale che ri‑serializza.** È la
correzione strutturale di W‑1, ed è un **cambio di protocollo**: deliberatamente
**non aperto** in questa campagna. La regola operativa nel frattempo è nel
`CLAUDE.md`: prima di aggiungere un campo a **qualunque** struct che il server
ri‑serializza, decidere se deve sopravvivere a un server vecchio — e se deve,
non può viaggiare lì.

**Non aggiungere mai una variante nuova a `UdpCandidateKind`.** I peer vecchi
non riescono a deserializzare una variante serde sconosciuta. Un forward statico
**è** una mappatura del router, e per questo `--udp-candidate` usa `RouterMapped`
invece di un tipo nuovo. Stessa ragione per cui `spray_role` è una `String` e non
un enum.

## 4. Dove guardare

| file | cos'è |
|---|---|
| [`../../nat/NAT_TRAVERSAL.md`](../../nat/NAT_TRAVERSAL.md) | il riferimento del traversal: §14‑17 (fasi 0‑3), §19‑20 (fasi 6‑7), §21.3b |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | **§54** (la sonda MTU e la capacità che il server cancella) |
| [`../evidenze/SECRET_STAGING_EVIDENCE_2026-09-11.md`](../evidenze/SECRET_STAGING_EVIDENCE_2026-09-11.md) | §4: il round bimodale del listener |
| [`../../../scripts/udp_nat_netns_test.sh`](../../../scripts/udp_nat_netns_test.sh) | la matrice NAT, `T-NAT-DIAG-ROUND`, `T-NAT-SPRAY-*` |
