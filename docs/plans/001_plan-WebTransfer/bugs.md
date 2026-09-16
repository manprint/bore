# Bug ledger — 001 Web Transfer

Defects found outside a sub-phase's own scope, one row per bug. IDs are never
reused. A row exists so a later `verify` can tell an intentional diff from an
unexplained one; `STATE.md` §4 carries the one-line version.

| ID | Date | Symptom | Cause | Fix | Verification | Commit |
|----|------|---------|-------|-----|--------------|--------|
| B-A001 | 2026-09-17 | Un server REALE avviato con `--web-transfer-base-url` serve le room, ma `/admin/api/v1/config` risponde `web_transfer_enabled: false` e tutti e tredici i totali `null` — cioè «questo server non fa web transfer» mentre lo sta facendo. Le metriche, nello stesso momento, pubblicano i gauge come numeri: le due superfici si contraddicono | `main.rs` abilita il servizio con `set_web_transfer` mentre ha in mano la config risolta, poi costruisce il proprio `ConfigView` dai valori della CLI — che i limiti risolti non li conoscono — e lo installa con `set_config_view`, sovrascrivendo ciò che il servizio aveva pubblicato. Nessun test Rust lo vedeva: un test costruisce il `Server` direttamente e non installa mai una seconda view | i totali configurati vivono ora accanto al registry (`Server::web_transfer_view`, tipo `WebTransferConfigView`) e `set_config_view` li RIAPPLICA, quindi l'ordine delle due chiamate non può più cambiare ciò che un operatore legge. Il campo è `None` a servizio spento, che è ciò che fa pubblicare `null` e mai uno zero | unit `a_config_view_installed_later_keeps_the_web_transfer_totals` (red-checked: senza la riapplicazione legge `Bool(false)`, esattamente il difetto in produzione) + il gruppo `T-WEBUI-E2E` di `scripts/admin_dashboard_test.sh` che l'ha trovato, ora 31 PASS / 0 FAIL su un server vero | uncommitted |
