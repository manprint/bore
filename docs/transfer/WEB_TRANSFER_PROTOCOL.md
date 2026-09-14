# Web Transfer Protocol v1 (normative)

> **Status:** normative for `bore transfer web` v1. Breaking changes require v2.
> **Fixtures:** [`../../tests/fixtures/web_transfer/v1/`](../../tests/fixtures/web_transfer/v1/)
> — every example below is executed by `cargo test --all-features --lib
> web_transfer` and `npm run check --prefix web/transfer`. Prose-only examples
> are non-normative; on conflict the fixtures and the byte tables win.

## 1. Identifiers and room URL

All IDs are canonical lowercase hex, fixed width, no prefix/surrounding
whitespace: `RoomId`/`PeerId`/`OfferId`/`TransferId`/`AttemptId` 32 chars
(16 bytes); `MemberToken`/`OwnerToken`/`RoomKey` 64 chars (32 bytes);
`RelayTicket` 32 chars (16 bytes); `requestId` 32 chars (16 bytes).
Uppercase, short, long or non-hex input is rejected (`INVALID_MESSAGE`).

Room URL (capability; secrets live in the fragment, never the query):

```text
https://<authority>/transfer/<room:32hex>#m=<member:64hex>&k=<key:64hex>
```

The fragment never reaches the server (RFC 3986 §3.5): the browser moves
`m`/`k` into `sessionStorage` on load and scrubs them from the address bar.
`m` authenticates the peer on the control WebSocket; `k` never leaves the
browser (payload key derivation only). The CLI owner holds a separate
`OwnerToken` and never sees `m`/`k`.

## 2. Control envelopes (WebSocket text, `bore-transfer-v1`)

One JSON object per message, UTF-8, at most
`WEB_TRANSFER_MAX_CONTROL_BYTES` (320 KiB) — larger input is dropped before
parsing. Top-level shape is exactly `{v,type,requestId?,body}`; unknown
top-level fields are rejected. `v` must be `1` or the peer answers
`UNSUPPORTED_VERSION` and stays connected.

Client → server `type` values:

| type | requestId | body |
|------|-----------|------|
| `hello` | no | `{memberToken, displayName?}` |
| `ping` | no | `{}` |
| `peer.rename` | yes | `{displayName}` |
| `offer.publish` | yes | `{offerId, manifest, mac}` |
| `offer.withdraw` | yes | `{offerId}` |
| `transfer.request` | yes | `{offerId, entryIds, selectionDigest, mode, resume?}` |
| `transfer.source_ready` | yes | `{transferId, attemptId}` |
| `transfer.reject` | yes | `{transferId, code?}` |
| `rtc.offer` | yes | `{transferId, attemptId, sdp}` |
| `rtc.answer` | yes | `{transferId, attemptId, sdp}` |
| `rtc.ice` | yes | `{transferId, attemptId, candidate}` |
| `transfer.direct_ready` | yes | `{transferId, attemptId}` |
| `transfer.direct_failed` | yes | `{transferId, attemptId, reason?}` |
| `transfer.cancel` | yes | `{transferId, reason?}` |
| `transfer.progress` | yes | `{transferId, attemptId, receivedBytes}` |
| `transfer.complete` | yes | `{transferId, attemptId, root}` |

Server → client `type` values (`ack`/`error` echo the client's `requestId`):

| type | body |
|------|------|
| `welcome` | `{peerId, roomId}` |
| `snapshot.begin` | `{}` |
| `snapshot.peer` | `{peerId, displayName?}` |
| `snapshot.offer` | `{peerId, offerId, manifest, mac}` |
| `snapshot.end` | `{}` |
| `ack` | `{requestId, result?}` |
| `error` | `{requestId, code, message?}` |
| `pong` | `{}` |
| `peer.joined` | `{peerId, displayName?}` |
| `peer.renamed` | `{peerId, displayName?}` |
| `peer.left` | `{peerId}` |
| `offer.added` | `{peerId, offerId, manifest, mac}` |
| `offer.removed` | `{peerId, offerId}` |
| `transfer.incoming` | `{transferId, offerId, fromPeerId, attemptId}` |
| `transfer.direct_start` | `{transferId, attemptId, attemptNumber, role, iceServers, deadlineMs}` |
| `transfer.path_commit` | `{transferId, attemptId, path}` (`path` is `direct` or `relay`) |
| `transfer.relay_ticket` | `{transferId, attemptId, ticket}` |
| `transfer.cancelled` | `{transferId, byPeerId}` |
| `transfer.completed` | `{transferId, root}` |
| `room_closed` | `{reason}` |

Error `code` values (fixed set; anything else is `INTERNAL`):

```text
UNSUPPORTED_VERSION ROOM_UNAVAILABLE UNAUTHORIZED INVALID_MESSAGE RATE_LIMITED
LIMIT_EXCEEDED OFFER_NOT_FOUND OFFER_CHANGED TRANSFER_NOT_FOUND NOT_PARTICIPANT
SOURCE_OFFLINE SOURCE_CHANGED DIRECT_FAILED RELAY_BUSY STORAGE_QUOTA CANCELLED
INTERNAL
```

i.e. exactly: `UNSUPPORTED_VERSION`, `ROOM_UNAVAILABLE`, `UNAUTHORIZED`,
`INVALID_MESSAGE`, `RATE_LIMITED`, `LIMIT_EXCEEDED`, `OFFER_NOT_FOUND`,
`OFFER_CHANGED`, `TRANSFER_NOT_FOUND`, `NOT_PARTICIPANT`, `SOURCE_OFFLINE`,
`SOURCE_CHANGED`, `DIRECT_FAILED`, `RELAY_BUSY`, `STORAGE_QUOTA`,
`CANCELLED`, `INTERNAL`.

Idempotency: identical `(peer,requestId)` replays return the cached terminal
response; same offer ID plus byte-identical canonical manifest acks;
withdraw/cancel of an already-terminal object acks; a duplicate
`transfer.request` returns the same transfer ID/state; changed content for an
existing offer ID conflicts with `OFFER_CHANGED`.

## 3. Relay attach (WebSocket text, first message only)

On `/transfer/ws/relay/<room>/<transfer>` the first message within 10 s must
be text `relay.attach`:

```json
{"v":1,"peerId":"…","transferId":"…","attemptId":"…","role":"source","ticket":"…"}
```

`role` is `source` or `recipient`; `ticket` is a one-use 32-hex
`RelayTicket`. Only exact top-level fields `{v,peerId,transferId,attemptId,
role,ticket}` are accepted. Binary ciphertext frames follow (Phase 3).

## 4. Canonical JSON

Signing, hashing and digest inputs use canonical JSON:

- objects: keys sorted by Unicode code-point order, recursively;
- UTF-8, no insignificant whitespace;
- integers only within the JSON safe range; sizes/timestamps are decimal
  **strings**, never numbers;
- no duplicate keys (rejected before parsing completes).

## 5. Manifest v1

```json
{
  "offer": "<offer-id 32hex>",
  "mode": "single",
  "entries": [
    {"path": "hello.txt", "size": "11", "mtime": "1757779200",
     "chunks": ["<sha256hex of the 1 MiB chunk>"]}
  ]
}
```

- `mode` is `single` (one file) or `multi` (tree/ZIP source).
- `size`/`mtime` are decimal strings (Unix seconds for `mtime`).
- `chunks` holds one SHA-256 hex per 1 MiB logical chunk (last chunk short).
  At most `max_entries_per_offer` entries; manifest bytes at most
  `WEB_TRANSFER_MAX_MANIFEST_BYTES` (256 KiB).
- Path rules: `/`-separated segments, no leading/trailing `/`, no empty,
  `.` or `..` segments, no `\`, no control characters, NFC-normalized,
  each segment at most 255 bytes, whole path at most 4096 bytes.

Content root ("fixed rolling-root", order-sensitive):

```text
leaf_i   = SHA256(chunk_i)
root     = SHA256("bore-web-root-v1" || u64be(chunk_count) || leaf_0 || … || leaf_{n-1})
```

`chunk_count` is the entry's chunk count (`0` for an empty file, whose root is
`SHA256("bore-web-root-v1" || u64be(0))`). A transfer's root is the root of
its single entry (v1 transfers carry exactly one entry).

Manifest authentication: `mac = HMAC-SHA256(manifest_key,
canonical_manifest_bytes)` with

```text
manifest_key = HKDF-SHA256(ikm = room_key_raw,
                           salt = UTF8("bore-web-manifest-v1"),
                           info = room_id_raw, 32 bytes)
```

The server checks the MAC but never learns the room key.

## 6. Encrypted frames (direct DataChannel and relay, identical bytes)

One message is at most 32 KiB: 16-byte header + ciphertext + 16-byte GCM tag.
Plaintext fragments are at most 24 KiB (`DATA`) or exactly 8 bytes (`FINAL`).

Header (16 bytes, every numeric field big-endian):

| offset | size | field | value |
|--------|------|-------|-------|
| 0 | 4 | `magic` | `0x42575431` (`BWT1`) |
| 4 | 2 | `version` | `1` |
| 6 | 1 | `frame_type` | `1` = DATA, `2` = FINAL |
| 7 | 1 | `flags` | reserved, must be `0` |
| 8 | 4 | `seq` | fragment sequence, starts at `0`, strictly increasing |
| 12 | 4 | `body_len` | ciphertext+tag length, must equal actual trailing bytes |

Body: `ciphertext || tag` where `AES-256-GCM(key, nonce, aad=header,
plaintext)` uses:

```text
key   = HKDF-SHA256(ikm = room_key_raw,
                    salt = UTF8("bore-web-attempt-v1"),
                    info = transfer_id_raw || attempt_id_raw, 32 bytes)
nonce = u64be(seq) || u32be(0)   (12 bytes; unique per key via seq)
```

- `DATA` plaintext: 1..=24576 bytes of file content at `seq * 24576`.
- `FINAL` plaintext: exactly `u64be(total_plaintext_bytes)`.
- Decoders reject: wrong magic/version, unknown type, nonzero flags,
  `body_len` mismatch, trailing bytes, oversized fragment, reused/old
  `seq` outside the current attempt, and any GCM failure (wrong key,
  modified AAD/header, flipped bit). Buffers for `body_len` are capped
  before allocation: `body_len > 32768` fails without allocating.
- A frame decrypts only under its attempt key: frames from a stale attempt
  fail authentication and are discarded, never written.

## 7. Key hygiene

Room key, member/owner tokens and attempt keys never reach the server log,
admin state or error text. Attempt IDs, keys and sequences are fresh per
attempt; sequence numbers restart at `0` per attempt. Nonce reuse across
messages is impossible by construction (attempt-bound key + per-key unique
seq); reusing a sequence within an attempt is a decode error.

## 8. Bounds reference

Timings/sizes live as `WEB_TRANSFER_*` constants in `src/web_transfer.rs`:
protocol 1; heartbeat 20 s, liveness 60 s, reaper tick 500 ms, direct
deadline 10 s, relay attach 30 s, control send 10 s; manifest 256 KiB,
control 320 KiB, relay message/frame 32 KiB, plaintext fragment 24 KiB,
chunk 1 MiB, high/low water 4/1 MiB; SDP 64 KiB, ICE candidate 4 KiB × 128
per side; display name 48 chars; path 4096 B, segment 255 B.

## 9. Fixture generation

`tests/fixtures/web_transfer/v1/*.json` are generated, not handwritten:

```sh
node web/transfer/tests/unit/vectors.mjs > /tmp/vectors.json  # inputs → outputs
```

`crypto-vectors.json` carries `{inputs, expected}`; both the Rust codecs
(`src/web_transfer_protocol.rs`) and the JS mirror
(`web/transfer/src/{protocol,crypto}.js`) must reproduce `expected`
byte-for-byte. `T-WEB-E2EE-FIXTURE` runs the JS runner live and compares
against the Rust computation.
