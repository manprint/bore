# Fast link transfer

`curl -T file https://fast.<base>` → a one-shot download link, streamed through the server,
never stored. Server-side only: neither end runs `bore`. Plan and decision record:
[`docs/plans/004_plan-FastLinkTransfer/`](../plans/004_plan-FastLinkTransfer/overview.md)
(decision IDs `D*` and invariant IDs `I-*` below refer to it).

User-facing guide: the README section
[Fast link transfer](../../README.md#fast-link-transfer-curl--t--one-shot-link).
This document is the engineering reference: flow, state machine, responses, limits,
invariants and the tests that guard them.

## 1. Scenario

```sh
# server (vhost base domain + wildcard certificate already configured)
BORE_FAST_LINK_TRANSFER_ENABLED=true \
BORE_FAST_LINK_TRANSFER_VHOST=fast.bore.tld \
BORE_FAST_LINK_TRANSFER_AUTH='alice:s3cret' \
bore server --vhost-base-domain bore.tld --vhost-cert-file wild.pem --vhost-key-file wild.key

# uploader: a file (Content-Length) or a live stream (chunked)
curl -u alice:s3cret -T miofile.tar https://fast.bore.tld
sudo tar -cpf - myfolder | curl -N -u alice:s3cret -T - https://fast.bore.tld/myfolder.tar

# downloader: any HTTP client, once
curl -fO https://fast.bore.tld/k3v9q0x7m2a8d1zp/miofile.tar     # or wget, or a browser
```

The uploader's response body is plain text, one line per event:

```
https://fast.bore.tld/k3v9q0x7m2a8d1zp/miofile.tar
# waiting for the download (expires in 60 min); nothing is stored on the server
# download started
# done: 1073741824 bytes in 9.8 s (104.5 MiB/s)
```

## 2. Why it is always relayed

A direct path needs software at both ends to hole-punch; here both ends are stock HTTP
clients. The path is uploader → server → downloader: TLS terminated on the server on each
side, one hop, no multiplexing layer (the vhost relay adds a provider hop and yamux).

## 3. Components

| File | Role |
|------|------|
| `src/fast_link/mod.rs` | constants, `FastLinkServerArgs` → `resolve_server_config` (all validation, D4/D19/D20), `generate_id` (16 chars `[a-z0-9]`, CSPRNG rejection sampling, ~82 bits) |
| `src/fast_link/request.rs` | head parsing, `upload_framing`, upload/download targets, `preview_verdict`, `host_matches` |
| `src/fast_link/response.rs` | exact response bytes, `linger_close` / `abort_close` |
| `src/fast_link/framing.rs` | `BodyFramer`: Content-Length and chunked passthrough state machine |
| `src/fast_link/pump.rs` | the two-task streaming pump (D8) |
| `src/fast_link/session.rs` | `FastLink`: slots, upload/download tasks, handoff, re-arm, expiry, metrics |
| `src/server.rs` | `Server::set_fast_link` (D20 checks, label reservation, redirect port) and the unified control-port hook |
| `src/vhost.rs` | `ReservedVhostLabel` (D16), hooks in `handle_http` / `handle_https` |
| `src/prefixed.rs` | `ConnSecurity` — whether a stream type terminated TLS, decided by the type |

Routing (D17): on all three HTTP ingresses (dedicated vhost HTTP, dedicated vhost HTTPS,
unified control port) the hook runs on the head the existing code already read, BEFORE the
subdomain lookup, and only when the service is enabled (I-1). The `--max-conns` permit is
handed to `FastLink::serve` and travels with the stream, so a downloader handed to its
uploader's task keeps holding it.

## 4. Flow and state machine (D13)

```
PUT  → auth (head bytes only) → active permit → slot Waiting → 100 Continue (if asked)
     → 200 chunked + link line → wait: prefill up to the replay window, deadline
GET  → preview/HEAD answered without consuming → claim Waiting→Streaming under the slot
     lock → try_send(stream, permit) into the slot's 1-capacity handoff channel
U    → "# download started" → download head + replay → pump → outcome
```

States: `Waiting → Streaming → (Waiting on re-arm) → Closed`.

- Only the DOWNLOADER moves `Waiting → Streaming`, under the slot's mutex, then hands its
  stream over. A second GET while `Streaming` gets `409`.
- Only the UPLOADER leaves `Streaming` (→ `Waiting` on re-arm, → `Closed` otherwise).
- Expiry or an uploader abort racing a claim: `take_or_close` either closes a `Waiting` slot
  or reports the claim in flight, and the in-flight handoff is received (bounded by
  `HANDOFF_RECV_TIMEOUT`, 5 s) so a claimant is never dropped without an answer.
- `SlotGuard` (RAII) removes the slot and settles the gauges on every exit (I-8).

## 5. Responses

| Case | Answer | Consumes the link |
|------|--------|-------------------|
| `PUT` without / wrong Basic credential | `401` (before `100 Continue`, before any slot) | — |
| `PUT` with `Content-Length` **and** `Transfer-Encoding` | `400` | — |
| `PUT` with a `Transfer-Encoding` other than `chunked` | `501` | — |
| `PUT` with neither | `411` | — |
| `PUT` path with more than one segment, a query, or an invalid name | `400` | — |
| too many uploads (`--fast-link-transfer-max-active`) | `503` + `Retry-After: 30` | — |
| request head over 16 KiB / 64 headers | `431` / `400` | — |
| plain HTTP `GET`/`HEAD` | `308` → `https://<fast host>[:<https port>]<target>` | no |
| plain HTTP other methods | `403` | — |
| `GET`/`HEAD /` | `200` usage text | — |
| `GET /<unknown or closed id>` | `404` | — |
| `HEAD /<id>` | the download's headers, no body | no |
| `GET` from a known preview bot (User-Agent) | `200` generic HTML | no |
| `GET` with `Range` other than `bytes=0-` | `416` | no |
| `GET` while another download streams | `409` | no |
| methods other than `PUT`/`GET`/`HEAD` | `405` | — |

Error answers use a bounded linger close (flush, shutdown, drain ≤ 1 MiB / 2 s) so an
unread body cannot turn the answer into a TCP RST that the client never reads (D21).

Uploader outcomes (D10, I-4): success ends the chunked body with its terminator
(`curl` exits 0). Every other end — `# failed: …`, `# expired: …` — closes WITHOUT the
terminator, so `curl` exits 18 and a truncated transfer never looks complete.

## 6. Framing (D11)

- `Content-Length`: the downloader gets the same `Content-Length`; exactly that many body
  bytes are forwarded.
- `Transfer-Encoding: chunked`: the downloader gets `Transfer-Encoding: chunked` and the
  uploader's chunk framing passed through VERBATIM, validated by `BodyFramer` (size line
  ≤ 4096 bytes, CRLF discipline, non-empty trailers rejected) and never re-encoded.
- The body a head reader already read together with the head is fed to the framer first;
  it is never lost and never parsed as headers.

## 7. Preview protection (D2)

A chat link preview must not burn the one download:

1. User-Agent markers (case-insensitive substring): `slackbot`, `slack-imgproxy`,
   `discordbot`, `telegrambot`, `whatsapp`, `facebookexternalhit`, `facebot`,
   `meta-externalagent`, `twitterbot`, `linkedinbot`, `skypeuripreview`, `mattermost-bot`,
   `googlebot`, `bingbot`, `applebot`, `embedly`, `iframely`, `redditbot`, `bitlybot`,
   `pinterestbot`, `vkshare` → generic HTML, nothing consumed.
2. `Range` other than `bytes=0-` (Slack fetches "as little as it can" with ranges) → `416`.
3. The backstop that needs no list: the replay window. While the upload has not moved past
   its first 4 MiB (`REPLAY_WINDOW_BYTES`), every byte handed to a downloader is still in
   RAM; a download that drops then re-arms the link and the next downloader gets the replay
   first (I-3). Past the window a dropped download is final.

Limit: a scanner that downloads the whole file (mail "safe links" rewriters, for example)
consumes the link; no heuristic can tell it from a person.

## 8. The pump (D8, I-6, I-11)

Two tasks per transfer: R owns the uploader (read, TLS decrypt, framing, replay), W owns
the downloader (TLS encrypt, write, flush), so the two crypto halves run on different
cores. Between them `PUMP_DEPTH` (4) fixed buffers of `proxy_buffer_size()` (256 KiB
default, `BORE_PROXY_BUFFER_SIZE`) circulate through bounded channels: zero allocation in
steady state (no glibc mmap-threshold churn, H-18), end-to-end backpressure through the
channels and TCP. R coalesces the reads that are already ready (`now_or_never`) into one
buffer before handing it on: tokio-rustls yields about one TLS record (≤ 16 KiB) per read,
and one message per record would cost ~64 000 wakeups and flushes per second at 1 GB/s.
W flushes after every write (a TLS record left in the session would park a keep-alive
peer forever — the vhost lesson of 36cd70d). Never `tokio::io::copy` (8 KiB buffer).
No hashing on the server.

Cancellation: R cancels before dropping its sender; a panic in either task is re-raised
(`resume_unwind`), never swallowed. Stall bound: 600 s without progress on either side;
the pre-pump head + replay write to the downloader has the same bound, so a downloader
that connects and never reads cannot park the uploader.

## 9. Limits and memory (D15, I-2)

| Limit | Value |
|-------|-------|
| concurrent uploads (waiting + streaming) | `--fast-link-transfer-max-active`, default 32 (1..=4096) |
| wait for a downloader | `--fast-link-transfer-wait-timeout`, default 3600 s (1..=604800) |
| stall (no progress) | 600 s |
| request head | 16 KiB, 64 headers |
| chunk size line | 4096 bytes |
| in-flight handoff wait | 5 s |
| linger close | 2 s / 1 MiB |

RAM per transfer ≤ replay window (4 MiB) + `PUMP_DEPTH` × `proxy_buffer_size()` + one
wait buffer of `proxy_buffer_size()`. Worst case with defaults ≈ 32 × (4 MiB + 1.25 MiB).
Nothing is written to disk (T-FL-TRANSIT measures `write_bytes` of the server process).

## 10. Security

- Upload: HTTP Basic (`--fast-link-transfer-auth`), checked in constant time on the head
  bytes only (never on body bytes that happen to be buffered with it), before `100
  Continue` and before any slot or permit (I-7). Failures count `auth_failures_total`.
- Download: the link is a bearer credential (16 chars, ~82 bits). Anyone holding it can
  claim the one download.
- HTTPS only (D9, I-9): plain requests are redirected or refused; startup fails when no
  listener would serve the fast host over TLS (a vhost certificate under `mode: http`, or
  a vhost HTTPS port equal to a plain control port, does not count).
- The label of the fast host is reserved (D16, I-10) through
  `vhost::ReservedVhostLabel` (`Arc<OnceLock<String>>`, never a `VhostConfig` field): a
  native `HelloVhost` or an SSH `-R vhost/<label>` for it is refused with the reason.
- Logs never carry the credential, the filename or the full id — only its first four
  characters (D18). The admin API never publishes the credential.

## 11. Admin (D6)

`/admin/api/v1/config` → `fast_link`: `host`, `wait_timeout_seconds`, `max_active`,
`replay_window_bytes`. `/admin/api/v1/metrics` → `fast_link`: `waiting`, `streaming`,
`uploads_total`, `completed_total`, `failed_total`, `expired_total`, `rearmed_total`,
`previews_blocked_total`, `auth_failures_total`, `rejected_busy_total`, `bytes_total`.
Both are `null` when the service is disabled. The dashboard's Metrics panel shows a "Fast
Link" card (every value tested `!= null`, so a gauge at 0 still renders).

## 12. Invariants and their guards

| ID | Invariant | Guard |
|----|-----------|-------|
| I-1 | Disabled ⇒ every HTTP/vhost/control path unchanged | existing suites, `a_disabled_server_routes_the_fast_host_like_any_vhost`, T-FL-E12 |
| I-2 | No payload byte on disk; bounded RAM | T-FL-TRANSIT (`scripts/fast_link_perf.sh`) |
| I-3 | At most one completed download; re-arm only with the replay intact | `a_download_dropped_inside_the_window_rearms`, `a_download_dropped_past_the_window_fails_both`, T-FL-E6/E7 |
| I-4 | curl uploader exits 0 ⟺ the downloader received everything | session tests, T-FL-E1/E6/E8, T-FL-PW |
| I-5 | A truncated body never looks complete to the downloader | session tests, T-FL-E9 |
| I-6 | Every write flushed before waiting | `pump_writes_are_flushed_before_waiting` (red-checked) |
| I-7 | Auth before `100`, before any allocation, on head bytes only | `auth_failure_answers_401_before_any_continue`, `authorization_in_the_body_prefix_is_ignored` |
| I-8 | Slot removed and gauges settled on every exit | every session test (`slots_len() == 0`) |
| I-9 | TLS only | `plain_connections_are_refused_or_redirected`, `plain_http_frontend_refuses_uploads_and_redirects_downloads`, T-FL-E11 |
| I-10 | Nobody can register the fast label | `native_vhost_registration_of_the_fast_label_is_rejected`, `t_ssh_fast_link_label_is_reserved` |
| I-11 | Bandwidth ≥ 1.0 × the vhost relay on the same host | T-FL-PERF (`scripts/fast_link_perf.sh`) |

Acceptance scripts: `scripts/fast_link_e2e.sh` (T-FL-E1..E13, real curl and wget),
`scripts/fast_link_perf.sh` (T-FL-PERF, T-FL-TRANSIT), and the Playwright spec
`web/transfer/tests/e2e/fast-link.spec.mjs` (T-FL-PW, Chromium). All run in CI (`fast-link`
job and the existing browser job).

## 13. Defects found while building it

- A head reader returns the first body bytes with the headers; `vhost::extract_host_from_head`
  decoded the whole buffer as UTF-8, so a small binary `curl -T` (no `Expect` below 1 MiB)
  answered `502`. Fixed for every vhost route; guarded by the unit test
  `extract_host_ignores_the_body_read_with_the_head` and by T-FL-E11 (a plain `PUT`, where
  curl writes head and body in one segment — it read `502` before the fix). Over TLS curl
  writes them in separate records, which is why T-FL-E13 alone does not catch it.
- The SSH gateway drained its queued messages with `session.data` before the channel-open
  confirmation (which `accept()` only enqueues), so every line queued before a client
  opened its session channel was lost. Fixed by draining through the session handle;
  `t_ssh_fast_link_label_is_reserved` asserts the exact reason and is red-checked.

## 14. Measured bandwidth

`scripts/fast_link_perf.sh`, 2026-09-25, one workstation (Intel Core 7 240H, 16 threads,
Linux 7.0), release build, loopback, 1 GiB from tmpfs, `BORE_PROXY_BUFFER_SIZE=16M` for both
arms, interleaved `VHOST FAST FAST VHOST VHOST FAST`, downloader speed in MiB/s:

| Run | vhost relay (raw) | fast link (raw) | median ratio | server CPU s/GiB vhost → fast |
|-----|-------------------|-----------------|--------------|-------------------------------|
| 1 | 1335.9, 1389.4, 1435.1 | 1949.1, 1887.4, 1960.2 | 1.403 | 0.67 → 0.57 |
| 2 | 1258.0, 1530.7, 1488.4 | 1439.0, 1831.8, 2101.4 | 1.231 | 0.62 → 0.59 |
| 3 | 1539.3, 1684.7, 1189.5 | 2117.7, 2240.0, 1838.7 | 1.376 | 0.59 → 0.50 |

T-FL-TRANSIT on every run: server `write_bytes` delta **0**, RSS growth 20.8 / 43.8 /
25.3 MiB against the I-2 bound of 96 MiB for a 16 MB buffer (the payload is 1 GiB).

These are loopback numbers: they compare the two paths' own cost on one machine (the
fast link needs about 15 % less server CPU per GiB and moves 1.2–1.4× the bytes), not a
network's capacity. A real link will usually be the limit first.
