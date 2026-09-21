# Web Transfer Protocol v1 (normative)

> **Status:** normative for `bore transfer web` v1. Breaking changes require v2.
> **Fixtures:** [`../../tests/fixtures/web_transfer/v1/`](../../tests/fixtures/web_transfer/v1/) and the deterministic link vector [`../../tests/fixtures/web_transfer/link_v1.json`](../../tests/fixtures/web_transfer/link_v1.json)
> — every example below is executed by `cargo test --all-features --lib
> web_transfer` and `npm run check --prefix web/transfer`. Prose-only examples
> are non-normative; on conflict the fixtures and the byte tables win.

## 1. Identifiers and room URL

All IDs are canonical lowercase hex, fixed width, no prefix/surrounding
whitespace: `RoomId`/`PeerId`/`OfferId`/`TransferId`/`AttemptId` 32 chars
(16 bytes); `MemberToken`/`OwnerToken`/`RoomKey` 64 chars (32 bytes);
`RelayTicket` 32 chars (16 bytes); `requestId` 32 chars (16 bytes).
Uppercase, short, long or non-hex input is rejected (`INVALID_MESSAGE`).

Room URL (capability; the only accepted browser form):

```text
https://<authority>/transfer/#<seed22>
```

`seed22` is exactly 16 random bytes encoded as 22 unpadded Base64URL
characters (`A-Z`, `a-z`, `0-9`, `-`, `_`). Decoding is canonical: padding,
classic Base64 symbols, whitespace, percent escapes, Unicode, wrong decoded
length and non-zero trailing pad bits are rejected. The old
`32hex#m=<member>&k=<key>` form and the old room path are rejected before a
WebSocket is opened.

The fragment never reaches the server (RFC 3986 §3.5). It intentionally stays
visible in the address bar and browser history so reload and copy-link keep
working; the browser holds only the derived values in module memory. There is
no browser-storage recovery path, no fragment removal and no JavaScript crypto
fallback. A bare `/transfer/` may serve the shell, but it is not a room link
and opens no control socket.

The seed is the sole browser input. It is expanded with three independent
HKDF-SHA256 calls, each using the same IKM and salt but a different info label:

| Value | Salt | Info | Output | Use |
|---|---|---|---:|---|
| `RoomId` | `bore-web-transfer-link-v1` | `bore-web-transfer-room-id-v1` | first 16 bytes of 32 | registry and WebSocket route |
| `MemberToken` | `bore-web-transfer-link-v1` | `bore-web-transfer-member-token-v1` | 32 bytes | browser `hello` capability |
| `RoomKey` | `bore-web-transfer-link-v1` | `bore-web-transfer-room-key-v1` | 32 bytes | payload encryption and MAC |

The full HKDF output is 32 bytes for all three derivations; only `RoomId` is
truncated. The labels are domain separators, so the values are independent
even though the seed is shared. The link has 128-bit effective security: the
derived 256-bit values do not add entropy beyond the 16-byte seed.

The native owner holds a separate random `OwnerToken`; it is never derived
from the seed. Its owner-control handshake is protocol v2 and sends the
client-selected `RoomId`. That v2 applies only to native owner create/resume,
not to the browser payload/control protocol, which remains v1 with
`bore-transfer-v1`.

Room creation installs the requested derived `RoomId` atomically only when it
is vacant. A collision returns a generic room-unavailable error, does not
echo the seed or derived secrets, and leaves the existing room, owner lease
and relay mode unchanged. A collision is therefore safe even though the
derived room identifier is public.

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
| `transfer.request` | yes | `{offerId, entryIds, selectionDigest, mode, resume?}` — `mode` is `raw` (exactly one manifest entry, verified chunk by chunk against the manifest's digests) or `zip` (every manifest entry, delivered as ONE generated archive). `entryIds` are decimal strings sorted LEXICOGRAPHICALLY and unique (with eleven entries `"10"` precedes `"2"`), and the server refuses `4294967295` inside a manifest so it stays available as the archive's own entry ID |
| `transfer.source_ready` | yes | `{transferId, attemptId, selectionDigest}` (source re-attests the request's digest; mismatch is `SOURCE_CHANGED`) |
| `transfer.reject` | yes | `{transferId, code?}` |
| `rtc.offer` | yes | `{transferId, attemptId, sdp}` (recipient only, once, `sdp` 1..65536 bytes) |
| `rtc.answer` | yes | `{transferId, attemptId, sdp}` (source only, once, only after the offer) |
| `rtc.ice` | yes | `{transferId, attemptId, candidate, sdpMid?, sdpMLineIndex?}` (either side, 128 per side; `candidate` absent, `null` or `""` is the end-of-candidates marker and carries no other field; `candidate` <= 4096 bytes, `sdpMid` <= 64, `sdpMLineIndex` a `u16`) |
| `transfer.direct_ready` | yes | `{transferId, attemptId}` (only after that side's own signaling step) |
| `transfer.direct_failed` | yes | `{transferId, attemptId, reason?, resumeRanges?}` (`resumeRanges` is believed only from the recipient) |
| `transfer.cancel` | yes | `{transferId, reason?}` |
| `transfer.progress` | yes | `{transferId, attemptId, receivedBytes}` |
| `transfer.complete` | yes | `{transferId, attemptId, root}` |

Server → client `type` values (`ack`/`error` echo the client's `requestId`):

| type | body |
|------|------|
| `welcome` | `{peerId, roomId, displayName, limits, iceServers}` |
| `snapshot.begin` | `{revision}` |
| `snapshot.peer` | `{peerId, displayName?, revision}` |
| `snapshot.offer` | `{peerId, offerId, manifest, mac, revision}` |
| `snapshot.end` | `{revision}` |
| `ack` | `{requestId, result?}` |
| `error` | `{requestId?, code, message?}` (`requestId` absent only when the offending message carried none) |
| `pong` | `{}` |
| `peer.joined` | `{peerId, displayName?, revision}` |
| `peer.renamed` | `{peerId, displayName?, revision}` |
| `peer.left` | `{peerId, revision}` |
| `offer.added` | `{peerId, offerId, manifest, mac, revision}` |
| `offer.removed` | `{peerId, offerId, revision}` |
| `transfer.incoming` | `{transferId, offerId, fromPeerId, attemptId, mode}` — `mode` is additive and appended last; it tells the SOURCE which selection it is about to serve, so the source can recompute the selection digest (which covers the mode) itself. A server that predates it omits it, and `raw` is what such a server could only have meant |
| `transfer.direct_start` | `{transferId, attemptId, attemptNumber, role, iceServers, deadlineMs}` (`role` is the SDP role and is fixed: the recipient is always `offerer`, the source always `answerer`; `iceServers` are `stun:` URLs only, never TURN; `deadlineMs` is 10000) |
| `rtc.offer` / `rtc.answer` / `rtc.ice` | the counterpart's message, forwarded verbatim in a fresh envelope (no `requestId`). The server never parses, rewrites, stores or logs an SDP or a candidate |
| `transfer.direct_failed` | `{transferId, attemptId, reason, resumeRanges?}` — `reason` is always one of `ice-failed`, `channel-closed`, `send-error`, `unsupported`, `timeout`, `protocol`, `unknown` (a peer's own string is mapped into that set, never forwarded), `resumeRanges` is omitted when empty |
| `transfer.path_commit` | `{transferId, attemptId, path, resumeRanges?}` (`path` is `direct` or `relay`; `resumeRanges` is the recipient's verified `[start, end)` chunk ranges, present only when it holds a partial — it is how the source learns what to skip, since `transfer.request` never reaches it) |
| `transfer.relay_ticket` | `{transferId, attemptId, ticket}` |
| `transfer.progress` | `{transferId, attemptId, receivedBytes, path}` — the recipient's VERIFIED byte count, forwarded to the source alone. `receivedBytes` is a decimal string; `path` is `direct` or `relay` and is the SERVER's own, derived from the transfer state, never a value any peer sent |
| `transfer.cancelled` | `{transferId, byPeerId}` |
| `transfer.completed` | `{transferId, root}` |
| `room_closed` | `{reason}` |

The source cannot read the path off its own socket and cannot count what
the other side verified, so the forwarded `transfer.progress` is its only
source of both. Only the RECIPIENT may send one — it is the only party that
checked a digest against the manifest — and a report is dropped (acked, not
forwarded) when it names a stale attempt, arrives once the transfer is no
longer carrying, or carries `receivedBytes: 0`; more than the entry size is
an `INVALID_MESSAGE`. The recipient throttles its own reports to one per
500 ms or per MiB verified, whichever comes first, and only a chunk that
VERIFIED produces one.

### 2.1 Direct attempt, then relay

`transfer.source_ready` opens the DIRECT attempt and nothing else: the
server sends both peers a `transfer.direct_start` and takes **no** relay
slot. From there the order is fixed and the server enforces every step —

1. recipient → `rtc.offer` → source (once);
2. source → `rtc.answer` → recipient (once, after the offer);
3. both → `rtc.ice` → counterpart (128 each, end-of-candidates included);
4. each → `transfer.direct_ready` after its own step above;
5. on the SECOND ready the server sends `transfer.path_commit`
   `{path:"direct"}` to the recipient and then to the source. Only that
   message authorises the source to read the file.

Anything else ends the attempt: `transfer.direct_failed` from either peer,
the 10 s deadline, a cancel, a withdraw, a disconnect or the room closing.
The first three fall back automatically — same `transferId`, same
`selectionDigest`, same verified chunks, but a **fresh** `attemptId`,
attempt number, key and nonce sequence — and the transfer continues through
the Phase 3 relay admission with `transfer.relay_ticket` and
`transfer.path_commit {path:"relay"}`. The last three are terminal and win
over both the timer and any signaling still in flight. A message naming an
attempt that is no longer current is acked and ignored, never applied.

### 2.2 The direct channel itself

The server never sees this part: it is the contract the two pages keep with
each other, and a peer that breaks it ends the attempt with
`transfer.direct_failed` instead of carrying bytes nobody can verify.

| Property | Value |
|---|---|
| peer connections | exactly one per `(transferId, attemptId)`, built only after `transfer.direct_start` |
| ICE servers | the `iceServers` of that message, filtered to `stun:`/`stuns:` — never TURN, never a credential |
| data channels | exactly one; the recipient creates it, the source only accepts it |
| label / subprotocol | `bore-transfer-v1` / `bore-transfer-v1` |
| ordering | `ordered: true`, reliable (SCTP does the retransmission; the application adds none) |
| inbound messages | `ArrayBuffer` only — text or a `Blob` ends the attempt |
| plaintext fragment | `min(24576, pc.sctp.maxMessageSize - 64)`, floor 1024; below the floor the channel is unusable |
| backpressure | `bufferedAmountLowThreshold` 1 MiB; above 4 MiB queued the source stops reading the file and waits for `bufferedamountlow` |
| readiness | channel `open`, label/subprotocol/ordering correct, fragment size at or above the floor, and this peer's own SDP step done |

The frames on the channel are byte-for-byte the frames of §6: the same
header, the same per-attempt key, the same sequence starting at zero. The
transport is the only difference, which is what lets one attempt end and the
next continue from the chunks already verified.

**Fragmentation is a property of the peer, and 24 KiB is a protocol ceiling,
not a tuning constant.** `FRAME_MAX_PLAINTEXT` is 24576 and the relay's
32 KiB message cap is derived from it, so a larger fragment would need a wire
change on both paths and a new server bound; the peer's
`pc.sctp.maxMessageSize` is therefore the only thing that may make a fragment
SMALLER. A sender may not choose its own value: below the 1024-byte floor the
channel is declared unusable rather than used with a size the receive path was
not written for. The per-message cost of SCTP is real and was measured on
chromium — 8 KiB and 16 KiB fragments are both worse than 24 KiB
(`docs/transfer/WEB_TRANSFER_PERF.md`, 4.6) — so the ceiling is also the
winner, and the sizes are swept by the benchmark rather than assumed. One
transfer uses exactly one channel: spreading the fragments of a file over
several channels reorders it, because SCTP orders per stream.

`transfer.direct_failed.reason` is a FIXED code and never a peer's own
message: `ice-failed`, `channel-closed`, `send-error`, `unsupported`,
`timeout`, `protocol`, `unknown`. A transient `disconnected` that heals
inside 2 s is not a failure.

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

### 2.3 Resume, and why an archive resumes only by its prefix

`resumeRanges` on `transfer.path_commit` / `transfer.direct_failed` are
`[start, end)` CHUNK ranges (1 MiB chunks) the recipient has verified and
holds on disk. They are believed only from the recipient, and their meaning
differs by mode because the two modes have different evidence:

- `raw`: a chunk is identified by its index in ONE manifest entry and is
  verified against that entry's chunk digest, so any set of ranges is
  meaningful and the source skips exactly them. Ranges are coalesced, sorted
  and capped; a set too sparse to carry is reduced rather than dropped.
- `zip`: the archive is GENERATED, so a chunk has no manifest digest and no
  identity beyond its POSITION in a stream the source must regenerate. A
  resume is therefore a CONTIGUOUS PREFIX and nothing else: both ends apply
  the same reduction (`verifiedPrefix`) to the reported ranges and keep
  `[0, n)` alone. The source then regenerates from byte zero and HASHES every
  chunk — including the skipped ones, because the rolling root covers them —
  and sends only the chunks past `n`. So a zip resume saves NETWORK bytes, never
  source reads; the two counters are separate by design and the gate asserts
  both.

An archive's identity is the FINAL tuple (§6). The recipient persists
`{totalBytes, chunkCount, root}` the first time it completes an archive
attempt; on any later attempt for the same selection a tuple that disagrees
means the source's files changed under the resume, and the recipient answers
`SOURCE_CHANGED` — the verified prefix is KEPT, never deleted, and only an
explicit restart by the user discards it. The tuple is checked against what
the recipient has ON DISK (its committed chunk bytes), not against what
arrived on this attempt, because a resumed attempt legitimately carries less.

A partial is keyed by its selection, so `raw` and `zip` partials of the same
offer never collide and never resume into each other.

## 3. Relay attach (WebSocket text, first message only)

On `/transfer/ws/relay/<room>/<transfer>` the first message within 10 s must
be text `relay.attach`:

```json
{"v":1,"peerId":"…","transferId":"…","attemptId":"…","role":"source","ticket":"…"}
```

`role` is `source` or `recipient`; `ticket` is a one-use 32-hex
`RelayTicket`. Only exact top-level fields `{v,peerId,transferId,attemptId,
role,ticket}` are accepted. Binary ciphertext frames follow (Phase 3).

**The `FINAL` frame ends the leg, and nothing else does.** The server stops
forwarding the moment it has relayed a frame of type `2`, and the source
closes NEITHER transport after writing it. A WebSocket close is not an
end-of-stream marker here: a browser reports `bufferedAmount == 0` for a
message it has handed to the socket, not for one the peer has read, so a
source that closed after its last write truncated the leg — MEASURED on
WebKit, a source that had written all 8 388 613 bytes and then closed
delivered 7 087 168 of them, and the server read the short stream as the
source vanishing and failed a complete transfer for both peers. The recipient
verifies the transfer and reports it; the server tears the leg down from the
terminal transition. This is a property of the SERVER's pump, so an old
client that still closes early is not made correct by it — the rule is that
`FINAL` is the signal on BOTH transports, exactly as the direct path already
treated it.

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
  "label": "Photos",
  "kind": "file",
  "chunkSize": "1048576",
  "createdAt": "2026-09-14T12:00:00Z",
  "entries": [
    {"id": "0", "path": "hello.txt", "size": "11", "mtime": "1757779200",
     "chunks": ["<sha256hex of the 1 MiB chunk>"],
     "chunkCount": "1", "root": "<rolling root hex>"}
  ]
}
```

- `mode` is `single` (one file) or `multi` (tree/ZIP source).
- `label` is the catalog title (1..128 chars, NFC, trimmed, no controls).
- `kind` is `file` (one single file entry), `files` (flat files, `multi`)
  or `folder` (tree, `multi`, may hold directories).
- `chunkSize` is always `"1048576"` (1 MiB); `createdAt` is ISO-8601 UTC
  (`YYYY-MM-DDTHH:MM:SS[.frac]Z`, at most 32 bytes).
- `size`/`mtime`/`chunkCount`/`id` are canonical decimal **strings**, never
  numbers; `id` values are 0-based sequential in path-sorted entry order.
- `chunks` holds one SHA-256 hex per 1 MiB logical chunk (last chunk short).
  `chunkCount` always equals the chunk hash count. At most
  `max_entries_per_offer` entries; manifest bytes at most
  `WEB_TRANSFER_MAX_MANIFEST_BYTES` (256 KiB).
- A directory entry has empty `chunks`, `chunkCount`/`size` `"0"` and a null
  `root` (only under kind `folder`); every file entry carries the rolling
  root below, verified by the server at publish. An empty file is still a
  file: its root is the empty-input root, never null.
- Path rules: `/`-separated segments, no leading/trailing `/`, no empty,
  `.` or `..` segments, no `\`, no control characters, NFC-normalized,
  each segment at most 255 bytes, whole path at most 4096 bytes. Entries
  arrive strictly path-sorted; duplicates and NFC+casefold collisions are
  rejected (casefold is lowercase-over-NFC, exact for ASCII paths).

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

**The recipient MUST verify this tag before it requests anything.** The
manifest reaches a recipient THROUGH the server, which cannot compute the tag
— so the tag is the only thing that distinguishes a manifest a room member
wrote from one the relay invented. Skipping the check does not merely lose an
authenticity guarantee: the manifest carries the per-chunk digests and the
entry root, so a forged manifest makes every later per-chunk verification
succeed against the forgery and the download reports success while delivering
attacker-chosen bytes. A browser drops an announcement whose tag does not
verify (it never enters the catalog) and refuses the download outright if one
somehow reaches it.

## 6. Encrypted frames (direct DataChannel and relay, identical bytes)

One message is at most 32 KiB: 16-byte header + ciphertext + 16-byte GCM tag.
Plaintext fragments are at most 24 KiB (`DATA`); a `FINAL` is 8 bytes on a
`raw` transfer and 48 on a `zip` one.

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
- `FINAL` plaintext, `raw` transfer: exactly `u64be(total_plaintext_bytes)`,
  counting the plaintext that travelled on THIS attempt. On a resumed attempt
  the source skips the chunk ranges the recipient reported, so this is smaller
  than the entry size and the recipient checks it against what it asked to
  receive.
- `FINAL` plaintext, `zip` transfer: exactly 48 bytes,
  `u64be(total_plaintext_bytes) || u64be(chunk_count) || root[32]`. An archive
  is GENERATED, so its length, its chunk count and its root are in no manifest
  and can be checked against none: this frame is where the recipient learns
  what it should have received, and the AEAD over it is what makes the claim
  the source's own. The root is the same fixed rolling root the manifest uses
  for a file, over the archive's own 1 MiB chunks; the last chunk is short and
  is committed when this frame says no more are coming. Decoders accept these
  two lengths and no third.
- Decoders reject: wrong magic/version, unknown type, nonzero flags,
  `body_len` mismatch, trailing bytes, oversized fragment, reused/old
  `seq` outside the current attempt, and any GCM failure (wrong key,
  modified AAD/header, flipped bit). Buffers for `body_len` are capped
  before allocation: `body_len > 32768` fails without allocating.
- A frame decrypts only under its attempt key: frames from a stale attempt
  fail authentication and are discarded, never written.

## 7. Key hygiene

The seed and `RoomKey` never reach server logs, admin state, control frames or
error text. `MemberToken` crosses the browser control WebSocket only in the
initial `hello`; it is never logged, repeated or put in a URL after derivation.
The native `OwnerToken` is used only by owner create/resume and is compared by
hash, never echoed. Attempt IDs, keys and sequences are fresh per attempt;
sequence numbers restart at `0` per attempt. Nonce reuse across messages is
impossible by construction (attempt-bound key + per-key unique seq); reusing a
sequence within an attempt is a decode error.

The fragment is not a server request field, but it is intentionally observable
to the user agent: it can appear in the address bar, browser history,
screenshots and browser-extension or same-origin script observations. This
protocol does not promise secrecy against a compromised page, extension or
same-origin code.

### 7.1 A refusal says only "no"

Every pre-authentication refusal — a member token that is not hex, a token
that is hex and wrong, and a token that names a room the server does not
have — is answered with the SAME close code after the SAME uniform delay
(`WEB_TRANSFER_AUTH_FAIL_DELAY`). A client therefore cannot use its own
token to learn whether a room id exists, and the refusal log line carries a
coarse class and never the room or the token. This is a property of the
implementation as well as of the wire: no branch may return early, which is
what `t_web_auth_refusals_are_one_class` asserts as a floor (never as a
constant, because the network is not one).

### 7.2 Remote text is text, and only text

`displayName`, the offer `label` and every manifest `path` are chosen by
another peer. A renderer MUST place them in the document as text nodes, and
MUST strip the Unicode bidirectional CONTROL characters
(`U+061C`, `U+200E`, `U+200F`, `U+202A`–`U+202E`, `U+2066`–`U+2069`) before
doing so: they survive HTML escaping and let a name or a filename reorder
itself at render time, which is a lie told exactly where the reader decides
whether to download. The wire does not reject them — a name is opaque bytes
to the server, and rejecting would make a legitimate right-to-left name
unusable — so this is a client obligation, gated by
`remote_markup_and_bidi_do_not_execute_or_spoof_controls` and by the browser
half of `T-WEB-XSS-CSRF`.

### 7.3 Room-link derivation

The browser must use native WebCrypto HKDF with the exact table in §1. A
missing or failing `deriveBits` implementation is a pre-WebSocket unsupported
browser error; the page must preserve the fragment and construct no control
socket. No fallback KDF, storage recovery or server lookup is permitted.

## 8. Bounds reference

Timings/sizes live as `WEB_TRANSFER_*` constants in `src/web_transfer.rs`:
protocol 1; heartbeat 20 s, liveness 60 s, reaper tick 500 ms, direct
deadline 10 s, relay attach 30 s, control send 10 s, relay admit 30 s
(`WEB_TRANSFER_RELAY_ADMIT_TIMEOUT`), relay ticket 30 s
(`WEB_TRANSFER_RELAY_TICKET_SECS`), terminal record 5 min
(`WEB_TRANSFER_TERMINAL_RETENTION`); manifest 256 KiB,
control 320 KiB, relay message/frame 32 KiB, plaintext fragment 24 KiB,
chunk 1 MiB, high/low water 4/1 MiB; SDP 64 KiB, ICE candidate 4 KiB × 128
per side with `sdpMid` 64 B; display name 48 chars; path 4096 B,
segment 255 B.

## 9. Fixture generation

The deterministic short-link vector is
`tests/fixtures/web_transfer/link_v1.json`; the wire and payload vectors remain
under `tests/fixtures/web_transfer/v1/`. These fixtures are deterministic and
must not be regenerated with a fresh random seed on each run.

```sh
node --test web/transfer/tests/unit/crypto.test.mjs
```

The crypto test reads `link_v1.json`, checks the Rust/browser constants and
derivation, and uses independent `node:crypto.hkdfSync` as the oracle for the
three outputs. Both the Rust implementation
(`src/web_transfer_protocol.rs`) and the JS mirror
(`web/transfer/src/crypto.js`) must reproduce the expected values byte-for-byte.
`T-WEB-E2EE-FIXTURE` runs the JS side live and compares against the Rust
computation. The fixture is the normative source for the seed, salt, labels,
truncation and expected hex values.

## 10. Error table

Every `error` envelope carries one of the codes in §2. What a client should
DO with it is the part a code alone does not say, so it is fixed here.

| Code | Meaning | Retry? |
|------|---------|--------|
| `UNSUPPORTED_VERSION` | the server does not speak this `v` | no — reload the page; the shell and the server ship together |
| `ROOM_UNAVAILABLE` | the room is gone, expired, or past its owner grace | no — the link is dead, a new room is needed |
| `UNAUTHORIZED` | the member token does not open this room | no — and the refusal is deliberately indistinguishable from an absent room (§7.1) |
| `INVALID_MESSAGE` | the message is malformed, over a cap, or out of order | no — a correct client never sees it |
| `RATE_LIMITED` | the peer's own bucket is empty | yes, after a pause; the bucket refills |
| `LIMIT_EXCEEDED` | a configured total is reached (offers, peers, metadata, transfers) | yes, once something is released; the operator's totals are on `/admin/api/v1/config` |
| `OFFER_NOT_FOUND` | the offer id names nothing in this room | no — the catalogue moved on |
| `OFFER_CHANGED` | the offer id exists with different content | no — re-read the catalogue and ask again |
| `TRANSFER_NOT_FOUND` | the transfer id names nothing | no |
| `NOT_PARTICIPANT` | the peer is neither source nor recipient of that transfer | no |
| `SOURCE_OFFLINE` | the offering peer left | yes, if it comes back — the offer is withdrawn when it does not |
| `SOURCE_CHANGED` | the source republished different bytes under the same offer | no — the request must be made again against the new offer |
| `DIRECT_FAILED` | the direct attempt is over | automatic — the SAME transfer falls back to the relay with a fresh attempt |
| `RELAY_BUSY` | no relay slot is free | yes — the slots are a configured total, and one frees as a transfer ends |
| `STORAGE_QUOTA` | the RECIPIENT's browser refused the write | no — the user must free space; nothing was written |
| `CANCELLED` | the other participant cancelled | no |
| `INTERNAL` | anything else | yes, once; a repeat is a defect |

## 11. Route and header contract

Exactly five routes exist under `/transfer/`, and nothing else is served:

| Route | Method | Answer |
|-------|--------|--------|
| `/transfer/<room-id>` | `GET`, `HEAD` | the room shell (an HTML page; the id must be 32 lowercase hex) |
| `/transfer/assets/{index.html,app.js,app.css,offer-worker.js,stage-worker.js}` | `GET`, `HEAD` | the embedded bundle |
| `/transfer/ws/control/<room-id>` | `GET` + `Upgrade` | the control WebSocket |
| `/transfer/ws/relay/<room-id>/<transfer-id>` | `GET` + `Upgrade` | the opaque relay leg |
| anything else under `/transfer/` | any | `400`, including a bare `/transfer/` — a room shell without a room id is malformed by construction, not a listing |

Both WebSocket routes require, and the server verifies:

- `Host` equal to the authority of the configured base URL;
- `Origin` EXACTLY the base URL's origin — not a suffix match, so
  `evil-bore.example.com` and `bore.example.com.evil.test` are both refused;
- the subprotocol `bore-transfer-v1`, echoed back on the 101.

`X-Forwarded-*` is never read. A reverse proxy must therefore pass `Host` and
`Origin` through unchanged; the base URL may still be the public HTTPS origin
while the proxy-to-bore hop is plain HTTP on loopback.

The shell is served with
`default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self';
img-src 'self' data:; worker-src 'self'; base-uri 'none'; form-action 'none';
frame-ancestors 'none'` — no `unsafe-inline`, no `unsafe-eval`, no `blob:`
worker. The single `data:` is the empty favicon, which exists only so a
browser does not request one.

## 12. Cleanup matrix

What releases what, and when. Every row is a release the server performs
itself; none of them waits for a peer to be polite.

| Event | Released |
|-------|----------|
| a control socket closes | that peer, its offers, its metadata budget, its live transfers (the counterpart is told), its rate buckets |
| a peer stops answering | same, after `WEB_TRANSFER_CTRL_TIMEOUT` (60 s), decided on the reaper tick against `last_recv` — never a `timeout(recv)` |
| the owner lease is DROPPED (the shell died, the process was killed) | nothing immediately: the room DETACHES and lives out the owner grace, so a reconnect resumes it |
| the owner closes explicitly | the room, its peers and its offers, at once — a later `hello` is refused, not told the room is gone |
| the owner grace expires | as above |
| a transfer reaches a terminal state | its relay slot and its tickets; the terminal RECORD is kept `WEB_TRANSFER_TERMINAL_RETENTION` (5 min) so a reconnecting peer learns the outcome instead of re-requesting |
| a relay leg ends (`FINAL`, or either side vanishing) | the leg, its buffers and its slot |
| a relay ticket is used, or 30 s pass | the ticket — one use, and the attempt it is bound to |
| a direct attempt fails or times out | the attempt and its key; the transfer survives and falls back |
| the server process exits | everything: nothing is persisted, so there is nothing to reclaim on the next start |

## 13. Versioning

`v` is `1` on every envelope and `bore-transfer-v1` is the subprotocol. The
rule is one sentence: **a breaking change gets a new version and a new
subprotocol; v1 is never silently reinterpreted.**

Concretely, within v1:

- a field may be ADDED to a message, and it must be appended last and
  optional, so a peer that does not know it behaves exactly as before;
- a field's MEANING may not change, and a field may not become required;
- an error code may be added; a client treats an unknown code as `INTERNAL`;
- a limit may be raised or lowered by configuration — limits are operator
  policy, published on `/admin/api/v1/config`, not protocol.

Anything else — a removed field, a changed unit, a reordered handshake, a new
frame type with different framing — is v2, announced as the subprotocol
`bore-transfer-v2`, and a v1 client is answered `UNSUPPORTED_VERSION` rather
than served something that looks like v1 and is not.
