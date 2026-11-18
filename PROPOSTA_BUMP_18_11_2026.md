# Proposta bump toolchain + librerie — bore-forked — 18/11/2026

Repository: `/mnt/fabio/dati/Git/Github-manprint/bore-forked`
Versione crate: `bore-cli 1.4.0-rc.1`

Stesura: opencode — DeepSeek V4.1 Flash (provider opencode-go).
Revisione: Claude Code — Claude Opus 5.5, 18/11/2026. Ogni affermazione è stata
ricontrollata su lockfile, sorgenti delle crate scaricate da crates.io, API GitHub,
npm e `cargo audit`. Le correzioni sono marcate **[CORR]**, le aggiunte **[NUOVO]**.
L'esecuzione e i gate sono riportati in §9.

---

## 0. Esito della verifica (sintesi)

Confermato: toolchain 1.100.0 = ultima stable; 48 aggiornamenti semver-compatibili;
18 crate ferme per requirement; versioni ultime e MSRV di tutte le crate della tabella;
bug HOL ancora presente in russh 0.65.2; versioni GitHub Actions di checkout/node/artifact/cache/release/docker build+metadata.

Errori corretti:

1. **`rust-toolchain.toml` romperebbe la CI** (§1): i job cross installano i target sul toolchain `stable`, il file forzerebbe `1.100.0`, cioè un toolchain senza quei target.
2. **`rust-version = "1.89"` non era dimostrato** (§1): 1.89 è il minimo delle dipendenze, non la prova che bore compili su 1.89. Va verificato con quel compilatore.
3. **Salto netlink attribuito alla crate sbagliata** (§2.2): `netlink-packet-route 0.25.1` appartiene a `netconfig-rs 0.1.6`; `route_manager` era già su 0.33.0 e passa a 0.34.0.
4. **Duplicati** (§2.3): `base64 0.23` da solo NON elimina `0.22.1` (lo usa anche `attohttpc` via `igd-next 0.16`); `nix` ha 3 copie, non 2.
5. **Stime "file toccati" gonfiate** (§3): sha2 8 file (non 25), hmac 2 (non 10), yamux 1 (non 17), base64 2 (non 7). Fase sha2+hmac: costo medio-basso, non "alto, circa 35 file".
6. **`dashmap 6` non è un fork**: è lo stesso progetto upstream (`xacrimon/dashmap`).
7. **argon2/password-hash 0.6 sottostimati**: API PHC spostata nella crate `phc`, trait generici, `password-hash 0.6.0` è YANKED.
8. **Rebase russh sopravvalutato** (§4): il diff upstream 0.65.0→0.65.2 non tocca i file del delta HOL tranne due, in hunk disgiunti. Ma è **necessario per sicurezza**: sblocca `rsa 0.10.0` stable, che corregge RUSTSEC-2023-0071.
9. **GitHub Actions** (§5.1): `setup-buildx`, `setup-qemu`, `login` sono indietro di un major (dati "ok"); `setup-go` ultima = v7.1.0.
10. **Web** (§5.2): nessuno screenshot baseline da rifare; `is_nfc` è lato Rust, non zip.js; `dist/` committata va ricostruita.
11. **`serde_yaml`** (§3): i due sostituti proposti sono fermi dal 2024 e uno tiene `unsafe-libyaml` (RUSTSEC-2024-0449). Sostituto scelto: `serde-saphyr` (pure Rust, attivo).

Mancava del tutto:

- `cargo audit` (§3.2): 1 vulnerabilità (`rsa` Marvin) + `rustix` RUSTSEC-2026-0486 + 3 crate unmaintained.
- Immagini Docker datate (§5.3): `alpine:3.20` (EOL 2026-04) e `debian:bookworm-slim` (oldstable).
- `nix` va bumpata anche in `crates/bore-android-tun`.
- yamux 0.14.1 contiene anche un fix di overflow del credito di finestra (header malformato).

---

## 1. Toolchain di sviluppo

| Voce | Stato rilevato |
|---|---|
| `rustc` / `cargo` | 1.100.0 (rustc 2026-11-09, cargo 2026-11-05) — confermato |
| Verifica aggiornamenti | `rustup check` → `stable-x86_64-unknown-linux-gnu - up to date: 1.100.0` |
| `rust-toolchain.toml` / `rust-toolchain` | assente |
| CI GitHub | `dtolnay/rust-toolchain@stable` in 18 punti; con `targets:` in ci.yml:425, 641, 688, mean_bean_ci.yml:33, mean_bean_deploy.yml:89 |
| Docker | `FROM rust:alpine` (tag mobile, non pinnato) |
| Edition | root + `crates/bore-alloc` + `crates/bore-wintun` + `crates/bore-android-tun` = 2021; `crates/russh` = 2024 |
| `rust-version` (MSRV) | non dichiarato nel root; `crates/russh` dichiara 1.89 |
| MSRV massima richiesta dalle dipendenze | 1.89 |

MSRV per dipendenza (ultima versione disponibile):

| Crate | MSRV ultima |
|---|---|
| russh 0.65.2 (vendored) | 1.89 |
| uuid 1.29.3 | 1.89 |
| igd-next 0.18.0 / rcgen 0.14.10 / sha2 0.11.1 / time 0.3.55 | 1.88 |
| **[NUOVO]** rsa 0.10.0 | 1.87 |
| argon2 0.6.0 / password-hash 0.6.1 / **[NUOVO]** pkcs1 0.8.0 | 1.85 |
| clap 4.6.8 / hyper-util 0.1.21 / rstest 0.28 / hmac 0.13 / tokio-tungstenite 0.30 / tokio 1.54.1 | 1.85 |

La toolchain non ha nulla da bumpare.

Interventi di igiene:

1. **[CORR] `rust-toolchain.toml`: non aggiungerlo così.** `dtolnay/rust-toolchain@stable` con `targets:` installa i target cross (musl, android, windows, macOS) sul toolchain `stable`. Un file che fissa `channel = "1.100.0"` fa usare a rustup un toolchain diverso, senza quei target: i job cross falliscono con `can't find crate for core`. Un pin funziona solo se coordinato su tre fronti: file + 18 step CI `@1.100.0` + `rust:1.100-alpine` nei Dockerfile, con un bump manuale ogni 6 settimane. Decisione: resta `stable` mobile; la soglia minima la dichiara `rust-version`.
2. **[CORR] `rust-version`:** 1.89 è il minimo imposto dalle dipendenze, ma non prova che il codice di bore compili su 1.89. Si dichiara solo dopo `cargo +1.89.0 check` su tutte le feature (esito in §9).
3. Edition 2024: fuori scope di un bump (tocca tutto il crate). Da fare in un task separato.

---

## 2. Aggiornamenti dentro i requirement attuali (nessuna modifica a Cargo.toml)

Comando: `cargo update --dry-run --verbose`. Confermato: **48 pacchetti aggiornabili** e **18 pacchetti fermi** per requirement.

### 2.1 Elenco dei 48 aggiornabili (semver-compatibili)

```
aes 0.9.3->0.9.4            bumpalo 3.20.3->3.20.4       bytes 1.12.1->1.12.2
cc 1.10.0->1.11.2           clap 4.6.7->4.6.8            clap_derive 4.6.7->4.6.8
clap_lex 1.1.1->1.1.2       const-oid 0.10.2->0.10.3     crc32fast 1.5.3->1.5.5
crypto-bigint 0.7.10->0.7.11 ctutils 0.4.6->0.4.8        encoding_rs 0.8.42->0.8.44
errno 0.3.14->0.3.15        flate2 1.1.10->1.1.11        inout 0.2.5->0.2.6
itoa 1.0.18->1.0.19         js-sys 0.3.106->0.3.107      libloading 0.9.0->0.9.1
log 0.4.34->0.4.35          mio 1.2.4->1.2.5             ml-kem 0.3.3->0.3.4
module-lattice 0.2.3->0.2.4 netconfig-rs 0.1.6->0.1.7    netlink-packet-route 0.25.1->0.34.0
nix 0.31.3->0.31.4          rand 0.10.3->0.10.4          rand_pcg 0.10.2->0.10.3
regex-automata 0.4.18->0.4.19 regex-syntax 0.8.11->0.8.12 route_manager 0.2.17->0.2.18
rustix 1.1.5->1.1.6         rustls-pki-types 1.15.1->1.16.0 rustls-webpki 0.103.16->0.103.17
smallvec 1.16.4->1.16.6     syn 3.0.9->3.0.10            tun-rs 2.8.14->2.8.18
utf8_iter 1.0.4->1.1.0      uuid 1.29.0->1.29.3          wasm-bindgen(+macro,+macro-support,+shared) 0.2.129->0.2.130
wasm-bindgen-futures 0.4.79->0.4.80 web-sys 0.3.106->0.3.107 webbrowser 1.2.5->1.2.7
webpki-roots 1.0.11->1.0.12 zeroize 1.9.1->1.9.2
```

Aggiunto: `utf_types 1.0.0`. Rimossi: `netlink-packet-core 0.8.2`, `netlink-sys 0.8.8`, `paste 1.0.15`.

**[NUOVO]** Tra i 18 fermi, tre mancavano dalla tabella §3:
- `generic-array 0.14.7` (0.14.9 disponibile): bloccata da `crypto-common 0.1.7` (`generic-array = "=0.14.7"`). Non si interviene direttamente; sparisce quando `digest 0.10` esce dal grafo (fase 4 + `tungstenite 0.30`).
- `rsa 0.10.0-rc.18` → `0.10.0` e `pkcs1 0.8.0-rc.4` → `0.8.0`: bloccate dai pin `=` di russh 0.65.0 (vedi §4).

### 2.2 Punto di attenzione nel blocco patch — **[CORR]**

La riga di cargo `netlink-packet-route 0.25.1 -> 0.34.0` è una rinomina a livello di lockfile: in realtà si muovono **due** crate (verificato sul lock dopo l'update):
- `netconfig-rs 0.1.6 -> 0.1.7`: `netlink-packet-route 0.25.1 -> 0.33.0`, `netlink-packet-core`/`netlink-sys 0.8 -> 0.9`, esce `paste`. È il salto grosso;
- `route_manager 0.2.17 -> 0.2.18`: `netlink-packet-route 0.33.0 -> 0.34.0`.
Entrambe sono dipendenze **transitive di `tun-rs`**: bore non le importa. Il rischio è confinato alla configurazione device/indirizzi/rotte di `tun-rs` (feature `vpn`, Linux).
Gate: harness VPN netns (seriale). In più, la rimozione di `paste` chiude l'avviso RUSTSEC-2024-0436 e `rustix 1.1.6` corregge RUSTSEC-2026-0486.

### 2.3 Duplicati nel grafo — **[CORR]**

| Crate | Copie | Chi le tiene | Come si riducono |
|---|---|---|---|
| nix | **3** (0.29.0, 0.30.1, 0.31.3) | bore + `bore-android-tun` / `netconfig-rs` (anche 0.1.7) / `tun-rs` + `russh-cryptovec` | bump a 0.31 in **root e `bore-android-tun`** → restano 2 copie (0.30.1 resta per `netconfig-rs`) |
| base64 | 2 (0.22.1, 0.23.1) | bore **e `attohttpc 0.30.1` (via `igd-next 0.16`)** / `pem 4` | **serve `base64 0.23` + `igd-next 0.18`** (in 0.18 `attohttpc` è opzionale sotto `io_sync`, bore usa `default-features = false`) |
| digest / generic-array / sha1 | 2 / 2 / 2 | `sha2 0.10`, `hmac 0.12`, `tungstenite 0.28` (sha1 0.10) | fase 4 + `tokio-tungstenite 0.30` |
| rand | 2 (0.9.5, 0.10.x) | `igd-next 0.16`, `tungstenite 0.28`, `yamux 0.13`, `quinn-proto` | `quinn-proto` la tiene comunque |
| hashbrown | 2 | `dashmap 5` | `dashmap 6` |
| syn / getrandom / thiserror / synstructure / cpufeatures | 3 / 3 / 2 / 2 / 2 | transitive | non azionabili da bore |

---

## 3. Bump che richiedono modifica del requirement — **[CORR] conteggi file e note**

Conteggio: file `.rs` in `src/`, `tests/`, `examples/`, `crates/bore-*` che importano la crate.

| Crate | Lock | Target | MSRV | File | Note |
|---|---|---|---|---|---|
| `base64` | 0.22.1 | 0.23.1 | 1.71 | **2** (`src/web_transfer_protocol.rs`, `tests/fast_link_test.rs`) | dedup solo insieme a `igd-next 0.18` |
| `dashmap` | 5.5.3 | 6.2.1 | 1.65 | **13** | **[CORR] non è un fork**: stesso upstream. Bore usa solo `DashMap` + `mapref::entry::Entry` (niente raw-api/shards). `7.0.0-rc2` esiste ma è prerelease |
| `yamux` | 0.13.10 | 0.14.1 | — | **1** (`src/mux.rs`) | fix waker su stream splittato (issue #232) + **[NUOVO] credito finestra con `checked_add` → protocol error invece di overflow**; id connessione non più casuale (via `rand`). Break API solo `frame::Header` `nonce→id` (non usato). L'invariante bore "mai splittare un `mux::Stream` fra task" resta |
| `sha2` | 0.10.9 | 0.11.1 | 1.88 | **8** (5 src + test) | API alta (`Sha256::new/update/finalize/digest`): costo basso |
| `hmac` | 0.12.1 | 0.13.0 | 1.85 | **2** (`src/auth.rs`, `src/holepunch.rs`) | `ring::hmac` altrove non è questa crate |
| `nix` | 0.29.0 | 0.31.4 | 1.69 | 10 + **`crates/bore-android-tun`** | 0.32.0 esiste: si resta su 0.31 per allinearsi a `tun-rs`/`russh-cryptovec` |
| `x509-parser` | 0.16.0 | 0.18.1 | 1.67 | 1 | isolato (`src/certinfo.rs`) |
| `tokio-tungstenite` | 0.28.0 | 0.30.0 | 1.85 | **3** | tungstenite 0.30: server rifiuta `Sec-WebSocket-Key` non conformi; passa a `sha1 0.11`/`rand 0.10` |
| `igd-next` | 0.16.2 | 0.18.0 | 1.88 | 2 | toglie `attohttpc` + `base64 0.22` + `rand 0.9` dal grafo |
| `argon2` | 0.5.3 | 0.6.0 | 1.85 | 1 | **[CORR] break reale**: feature `simple`→`password-hash`, niente `std` |
| `password-hash` | 0.5.0 | **0.6.1** | 1.85 | 1 | **[CORR] 0.6.0 YANKED**; tipi PHC spostati nella crate `phc`; trait generici su `H`; salt via `generate_salt`. Requisito: gli hash `$argon2id$` già emessi devono continuare a verificare |
| `windows-sys` | 0.52.0 | 0.61.2 | 1.71 | 1 | solo Windows; `HANDLE` passa da `isize` a puntatore (0.59). Oracolo = job CI Windows |
| `rstest` | 0.15.0 | 0.28.0 | 1.85 | 1 (dev) | |
| `async_zip` | 0.0.18 | 0.0.19 | — | 1 | |
| `serde_yaml` | 0.9.34+deprecated | **[CORR] `serde-saphyr 2.2`** | 1.89 | 2 | `serde_yaml_ng` (ultimo rilascio 2024-05) tiene `unsafe-libyaml` (RUSTSEC-2024-0449); `serde_norway` è fermo al 2024-12. `serde-saphyr` è pure Rust e attivo |
| `ssh-key` | `=0.7.0-rc.11` | **`=0.7.0-rc.12`** | — | 2 | pin imposto da russh: 0.65.2 pinna rc.12 |
| **[NUOVO]** `rsa` | 0.10.0-rc.18 | 0.10.0 | 1.87 | 0 (transitiva) | **RUSTSEC-2023-0071**; sbloccata da russh 0.65.2 |
| **[NUOVO]** `pkcs1` | 0.8.0-rc.4 | 0.8.0 | 1.85 | 0 (transitiva) | idem |

### 3.1 Crate già all'ultima (nessuna azione)

Confermato: `anyhow`, `blake3`, `fastrand`, `futures-lite`, `futures-util`, `hex`, `http-body-util`, `httparse`, `hyper`, `hyper-util`, `quinn`, `rcgen`, `ring`, `serde`, `serde_json`, `socket2`, `subtle`, `time`, `tokio`, `tokio-rustls`, `tokio-util`, `tracing`, `tracing-subscriber`, `unicode-normalization`, `url`, `lazy_static`, `tempfile`, `procfs`.

### 3.2 **[NUOVO]** `cargo audit` sul lock di partenza

| ID | Crate | Tipo | Risolto da |
|---|---|---|---|
| RUSTSEC-2023-0071 | `rsa 0.10.0-rc.18` | **vulnerabilità** (Marvin, timing) | fase 5 (russh 0.65.2 → `rsa 0.10.0`) |
| RUSTSEC-2026-0486 | `rustix 1.1.5` | unsound | fase 1 |
| RUSTSEC-2024-0436 | `paste` | unmaintained | fase 1 |
| RUSTSEC-2024-0449 | `unsafe-libyaml` | unmaintained | fase 6 (`serde-saphyr`) |
| RUSTSEC-2026-0543 | `chrono` | unmaintained | non azionabile (via `procfs 0.18`, già ultima, e `russh-util`) |

Nota: in SSH la chiave RSA firma e non decifra, quindi l'esposizione reale a Marvin è bassa. L'avviso però è una vulnerabilità aperta nel lock e la correzione costa poco.

---

## 4. Blocco critico: `russh` vendored — **[CORR] rischio rivalutato**

Stato: `crates/russh` = 0.65.0 + due delta locali (HOL per canale, upstream `Eugeny/russh#730` chiusa non mergiata; close-wake/abort I-SSH10 con `WindowSizeRef.closed`, vedi `crates/russh/HOL_FIX.md`).

Confermato: in 0.65.2 il read loop fa ancora `chan.send(ChannelMsg::Data { .. }).await` (`src/server/encrypted.rs:1514`, e lato client `src/client/encrypted.rs:611`). Il bug HOL non è risolto upstream.

**[CORR]** Il diff 0.65.0→0.65.2 è di 456 righe. Tocca `cipher/mod.rs`, `client/kex.rs`, `client/mod.rs`, `keys/*`, `negotiation.rs`, `server/kex.rs`, `server/mod.rs` e i test.
Il delta locale tocca `channels/*`, `client/encrypted.rs`, `client/mod.rs`, `server/encrypted.rs`, `server/mod.rs`, `server/session.rs`, `session.rs`.
Sovrapposizione: solo `client/mod.rs` e `server/mod.rs`, in hunk disgiunti. Il "rebase" equivale ad applicare il diff upstream sul vendored (rischio medio, non alto).
Contenuto upstream da prendere:
- server: strict-kex corretto quando il guess `first_kex_packet_follows` è sbagliato;
- client: `kex_done` chiamato solo dopo `check_server_key` (il segreto condiviso non arriva più all'handler per un server poi rifiutato);
- pin `rsa`/`pkcs1` rimossi (→ stable, chiude RUSTSEC-2023-0071), `ssh-key` → `=0.7.0-rc.12`, feature `compat-0_14` di `generic-array` rimossa.
Unico conflitto atteso: `tests/test_window_adjust.rs`, già modificato localmente (vedi "Local test fixes" in `HOL_FIX.md`).
Gate: `cargo test --manifest-path crates/russh/Cargo.toml`, `tests/ssh_gateway_test.rs`, `T-SSH-JUMP`, `sudo -n .../scripts/ssh_gateway_test.sh`.

---

## 5. Fuori dal crate Rust

### 5.1 GitHub Actions — **[CORR]**

| Action | Usata | Ultima release |
|---|---|---|
| `actions/checkout` | v4 (31) | v7.0.1 |
| `actions/setup-node` | v4 (3) | v7.1.0 |
| `actions/upload-artifact` | v4 (6) | v7.0.2 |
| `actions/download-artifact` | v4 (2) | v8.0.2 |
| `actions/cache` | v4 (1) | v6.1.0 |
| `actions/setup-go` | v5 (1) | **v7.1.0** |
| `softprops/action-gh-release` | v2 (5) | v3.0.4 |
| `docker/build-push-action` | v6 (3) | v7.4.0 |
| `docker/metadata-action` | v5 (4) | v6.2.0 |
| `docker/setup-buildx-action` | v3 (5) | **v4.4.1** (non "ok") |
| `docker/setup-qemu-action` | v3 (2) | **v4.4.0** (non "ok") |
| `docker/login-action` | v3 (5) | **v4.8.0** (non "ok") |
| `Swatinem/rust-cache` | v2 (17) | v2.9.3 (ok) |
| `taiki-e/install-action` | v2 (6) | v2.90.6 (ok) |
| `reactivecircus/android-emulator-runner` | v2 (2) | v2.38.0 (ok) |
| `dtolnay/rust-toolchain` | @stable (18) | ok |

Non verificabile in locale: la prova è la prima run CI completa dopo il push, che richiede autorizzazione esplicita.

### 5.2 Frontend `web/transfer` — **[CORR]**

| Pacchetto | Attuale | Ultima |
|---|---|---|
| `@zip.js/zip.js` | 2.15.0 | 2.25.0 |
| `@playwright/test` | 1.63.0 | 1.65.1 |
| `esbuild` | 0.28.2 | 0.28.2 (ok) |

- Non ci sono screenshot baseline (`toHaveScreenshot` assente): playwright non richiede ri-baseline.
- `is_nfc` e il manifest sono lato Rust (`unicode-normalization`); zip.js costruisce solo lo ZIP nel browser. Rischio reale: `src/zip-stream.js` importa un path interno (`@zip.js/zip.js/lib/zip-core-writer.js`).
- `web/transfer/dist/` è **committata e incorporata nel binario** (`build.rs`): dopo il bump va rigenerata con `npm run build`. Gate: `npm run check` + `cargo test --test web_transfer_test` + e2e playwright.

### 5.3 **[NUOVO]** Immagini Docker

| File | Attuale | Proposta |
|---|---|---|
| `docker/Dockerfile.ssh` | `alpine:3.20` (EOL 2026-04) | `alpine:3.24` |
| `docker/Dockerfile.client` | `debian:bookworm-slim` (oldstable) | `debian:trixie-slim` |
| `Dockerfile`, `Dockerfile.client` (root) | `rust:alpine`, `alpine:latest` (mobili) | invariati |

---

## 6. Piano — **[CORR]**

Gate per ogni fase: `cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo clippy --features vpn --all-targets -- -D warnings`, `cargo build --all-features`, suite test come CI (main, doc, ssh seriale, web_transfer seriale, transfer_link no-default), `cargo audit`. Harness netns seriali dove la fase tocca quell'area. Confronto contro la baseline pre-bump (§9): **zero regressioni**.

| Fase | Contenuto | Rischio |
|---|---|---|
| 1 | `cargo update` (48) + `rust-version` (solo se `+1.89.0` compila) | basso; netlink via `tun-rs` → harness VPN |
| 2 | `base64 0.23` + `igd-next 0.18` (insieme), `async_zip 0.0.19`, `x509-parser 0.18`, `windows-sys 0.61`, `nix 0.31` (root + android-tun), `argon2 0.6` (dipendenza diretta `password-hash` rimossa: `argon2` la ri-esporta) | medio (argon2) |
| 3 | `dashmap 6`, `yamux 0.14.1`, `tokio-tungstenite 0.30`, `rstest 0.28` | medio (yamux = trasporto) |
| 4 | `sha2 0.11` + `hmac 0.13` (8 file) | medio-basso |
| 5 | russh 0.65.2 (diff upstream sul vendored) + `ssh-key rc.12` + `rsa`/`pkcs1` stable | medio; chiude RUSTSEC-2023-0071 |
| 6 | `serde_yaml` → `serde-saphyr`; Actions; web; immagini Docker | basso |

---

## 7. Verdetto — **[CORR]**

Bump fattibile, nessun ostacolo di toolchain.
Il lavoro vero è in tre punti, non due:
- **russh 0.65.2**: oltre al HOL, è il fix della vulnerabilità `rsa`;
- **argon2/password-hash 0.6**: break API, con vincolo di compatibilità sugli hash esistenti;
- **yamux 0.14.1**: è il trasporto di tutti i tunnel. Diff piccolo ma va validato con gli harness.

`sha2`/`hmac` sono molto più leggeri del previsto (10 file contro i 35 stimati).

---

## 8. Comandi usati per la verifica (riproducibili)

```bash
rustup check
cargo update --dry-run --verbose
cargo audit
python3 -c 'tomllib su Cargo.lock'            # chi possiede ogni versione duplicata
curl -s https://index.crates.io/<xx>/<yy>/<crate>          # ultima versione + rust_version
curl -sL https://static.crates.io/crates/<c>/<c>-<v>.crate | tar xz   # diff sorgenti (yamux, russh, igd-next, ...)
curl -s https://api.github.com/repos/<owner>/<repo>/releases/latest
npm view <pkg> version
curl -s https://hub.docker.com/v2/repositories/library/<img>/tags
```

---

## 9. Esecuzione

### 9.0 Baseline (albero intatto, lock originale)

fmt, clippy (all-features e vpn), build, test main/doc/ssh/web/transfer_link no-default, win-clippy (`x86_64-pc-windows-gnu`): tutti verdi.
`test-russh`: un solo fallimento, ambientale (`russh_matches_openssh` lancia `/usr/sbin/sshd`, assente nella sandbox); con `openssh-server` installato passa.
`cargo audit`: RUSTSEC-2023-0071 (`rsa`) + 2 warning (`chrono`, `unsafe-libyaml`).

### 9.1 Fase 1 — `cargo update`

52 voci nel lock (le 48 della §2.1 più le transitive).
**Regressione trovata**: `clap_derive 4.6.8` sostituisce gli shadowing `let` con riassegnazioni `mut` nel codice generato; in debug lo stack del parse cresce e `tests::cli_vpn_static_requires_peer_addr` (percorso d'errore `requires` di `vpn connect`) va in stack overflow sul thread di test da 2 MiB (passa con 4 MiB).
Il binario non è toccato: `main` esegue `Args::parse()` su un thread da 8 MiB dichiarato (fix Windows già esistente).
Correzione: i test che toccano la CLI girano su un thread da 8 MiB come la produzione (`on_cli_stack` + `try_parse_args`/`parse_args` nel modulo test di `src/main.rs`, 41 chiamate).
Secondo overflow con la stessa causa: `transfer_listener_sender_cli_snapshots_unchanged` costruisce l'albero con `Args::command()`; ora passa da `on_cli_stack(Args::command)`.
`t_ssh_pub5_force_https_redirects_plain_http` fallito una volta: `PARAMS_GRACE` (5 s) scaduto con la sandbox sotto carico (installazione pacchetti in parallelo); rilanciato isolato.
`cargo audit` invariato rispetto alla baseline.
Commit `344d4d9`.

### 9.2 Fase 2 — `base64`, `igd-next`, `async_zip`, `x509-parser`, `nix`, `windows-sys`, `argon2`

- `argon2 0.6`: `PasswordHash`/`PasswordHasher`/`PasswordVerifier` dalla radice di `argon2`; `hash_password(bytes)` genera il sale da sé (feature default `getrandom`); la dipendenza diretta `password-hash` è rimossa.
  Compatibilità hash esistenti: nuovo test `argon2_0_5_hashes_still_verify` (due hash prodotti da 0.5, password giusta accettata, sbagliata rifiutata, nuovi hash con lo stesso prefisso `$argon2id$v=19$m=19456,t=2,p=1$`).
- `igd-next 0.18`: `SearchOptions` è `#[non_exhaustive]` → `Default` + assegnazione di `timeout` (N-5 invariato: il `timeout` complessivo resta 2 s).
- `windows-sys 0.61`: `HANDLE` è un puntatore → `handle.is_null()`.
- Gate verdi; `cargo audit` invariato. Commit `f69b22e`.

### 9.3 Fase 3 — `dashmap`, `yamux`, `tokio-tungstenite`, `rstest`

- `tungstenite 0.30` rifiuta lato server una `Sec-WebSocket-Key` che non decodifica a 16 byte (RFC 6455 §4.1), chiudendo senza risposta.
  Due test (`t_web_handshake_admission`, `t_web_http`) inviavano una chiave da 12 byte: corretti gli helper di test (`tests/web_transfer_test.rs`, `tests/support/websocket.rs`). I browser e il client tungstenite generano chiavi conformi: nessun impatto sul prodotto.
- `yamux 0.14.1`: nessuna modifica al codice; test mux/tunnel verdi.
- Gate verdi; `cargo audit` invariato. Commit `0527a5c`.

### 9.4 Fase 4 — `sha2 0.11.1`, `hmac 0.13.0`

- `digest 0.11`: `new_from_slice` è passato da `Mac` a `KeyInit` → import `KeyInit` in `src/auth.rs` e `src/holepunch.rs`. Nessun'altra modifica (gli `Array` di `hybrid-array` convertono in `[u8; 32]` e sono `AsRef<[u8]>`).
- Nessun duplicato `sha2`/`hmac`/`digest` nel grafo. Gate verdi; `cargo audit` invariato. Commit `ad16e0c`.

### 9.5 Fase 5 — russh vendored 0.65.2, `ssh-key =0.7.0-rc.12`, `rsa`/`pkcs1` stable

- `crates/russh` sostituito dal sorgente pristine 0.65.2 (crates.io) + i due delta locali (HOL #730, close-wake/abort I-SSH10).
  Le due patch in `scripts/patches/` sono rigenerate su 0.65.2 e verificate: pristine 0.65.2 + `russh-hol-730.diff` + `russh-close-wake-abort.diff` riproduce `crates/russh` byte per byte (esclusi `HOL_FIX.md`, `Cargo.lock`, `.cargo_vcs_info.json`).
- Corpo della patch HOL identico al precedente. La patch close-wake perde i due hunk sui test (`tests/auth_signer_failure.rs`, `tests/test_window_adjust.rs`): 0.65.2 ha già le firme `VerifiedPublicKey<'_>`.
- Root: `ssh-key = "=0.7.0-rc.12"` (allineato al requisito esatto di russh 0.65.2). Il lock passa a `rsa 0.10.0` e `pkcs1 0.8.0` stable e rimuove `generic-array 0.14`.
- Documenti aggiornati: `crates/russh/HOL_FIX.md` (base 0.65.2, rebase log, sezione "Local test fixes" rimossa), `docs/ssh-gateway/SSH_GATEWAY.md`, `docs/project/UPSTREAM_CHANGES.md`.
- Gate verdi. `cargo audit` pulito senza `--ignore`: RUSTSEC-2023-0071 chiuso. Il doctest russh `keys::load_public_key` legge `../files/id_ed25519.pub`, fixture del workspace git upstream assente nel tarball crates.io: fallisce identico sul pristine (in baseline non girava perché cargo salta i doctest dopo un test fallito). Annotato in `HOL_FIX.md`. Commit `6ee3a4d`.

### 9.6 Fase 6 — `serde-saphyr`, GitHub Actions, web, immagini Docker, MSRV

- `serde_yaml 0.9` → `serde-saphyr 2.2.0` (`default-features = false`, `deserialize`): `src/vhost.rs` e `tests/web_transfer_deploy_test.rs` (il test legge il compose come `serde_json::Value`). `unsafe-libyaml` esce dal grafo; `cargo audit` resta con il solo warning `chrono` (unmaintained).
- CI: rimosso `--ignore RUSTSEC-2023-0071` dal job audit; README aggiornato (host key RSA ora supportata senza advisory).
- Actions: checkout@v7, setup-node@v7 (Node 24), upload-artifact@v7, download-artifact@v8, cache@v6, setup-go@v7, softprops/action-gh-release@v3, docker/* (build-push@v7, metadata@v6, setup-buildx@v4, setup-qemu@v4, login@v4).
- Docker: `alpine:3.24` (`Dockerfile.ssh`, smoke: OpenSSH 10.3p1, autossh, sshpass, tini, entrypoint), `debian:trixie-slim` (`Dockerfile.client`, `scripts/transfer_link_container_test.sh` PASS).
- Web: `@zip.js/zip.js` 2.25.0, `@playwright/test` 1.65.1. zip.js 2.19 ha alzato il `chunkSize` interno da 64 KiB a 256 KiB e il read-ahead del lettore è circa 12 chunk: 3.0 MiB invece di 0.75 MiB sul red-check da 16 MiB (`tests/unit/zip.test.mjs`, soglia 2 MiB). Fix: `configure({ chunkSize: 64 KiB })` globale in `src/zip-stream.js` (`ZIP_CHUNK_BYTES`); store mode, byte dell'archivio invariati. Unit 228/228, `dist/app.js` riproducibile, e2e chromium 54 passati, 1 skipped, 1 non eseguito, 2 falliti solo per il proxy della sandbox (`fast-link`, end-of-candidates: verdi rilanciati senza variabili proxy).
- MSRV: `cargo +1.89.0 check --all-features --all-targets` verde → `rust-version = "1.89"` dichiarato; README lo cita.
- Commit `46b0bfe`.

### 9.7 Versione e CI

- `bore-cli` 1.4.0-rc.1 → **1.4.0-rc.2** (`Cargo.toml`, `Cargo.lock`). Nessun tag: il workflow `release` parte solo su tag `v*`.
- Nuovo invariante **W-2** (chunk size di zip.js) in `CLAUDE.md` e `docs/INVARIANTS.md`.
- Push su `dev`; esito CI sotto.
- CI su `4ffc146`: Docker, Docker SSH client, E2E (netns) verdi. `Workflow lint (actionlint)` rosso: `setup-go@v7` esporta `GOTOOLCHAIN=local` e actionlint v1.7.12 richiede go ≥ 1.25.0 → `go-version: "1.25"`. Job macOS cancellati da GitHub ("not acquired by Runner ... capacity constraints"): infrastruttura, rilanciati.
- CI su `911f962`: tutti verdi — CI, E2E (netns), Mean Bean CI (macOS inclusi), Mean Bean Deploy, Docker (GHCR); Docker SSH client verde su `4ffc146` (stessi input, non ri-triggerato). Unico rosso al primo tentativo: `T-WEB-CROSS direct webkit` ("webkit committed the wrong path"), flake noto, verde al rerun.
