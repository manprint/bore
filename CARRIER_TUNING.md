# CARRIER_TUNING

`--carriers` controls how many parallel relay TCP connections a tunnel may use.
It does not stripe one byte stream across many sockets. Instead, each new proxied
connection is assigned to one carrier, and the pool is picked round-robin.

That distinction matters:

- A single bulk flow usually sees little or no gain from raising `--carriers`.
- Many simultaneous relay flows often improve, because each carrier gets its own
  TCP congestion window and loss domain instead of sharing one yamux-over-TCP
  connection.
- Direct UDP / QUIC paths already use independent streams, so `--carriers`
  affects only the relay path or relay fallback.

## Core rules

1. `0` is special only on `bore transfer listener` and `bore transfer sender`.
   There it means `auto`.
2. Everywhere else, `0` and `1` both collapse to the original single-connection
   relay path.
3. `bore server --max-carriers` caps only server-managed carrier pools:
   public tunnels, secret providers, and vhost providers. It does not cap
   `bore proxy`, whose extra relay carriers are just extra consumer connections.
4. `--carriers 1` preserves the original path byte-for-byte.
5. More carriers help concurrency, not a lone stream.

## Applicability matrix

| Command | Flag / env | CLI default | Effective default when omitted | `0` meaning | Relay leg affected | Capped by `bore server --max-carriers` | Notes |
|---|---|---:|---|---|---|---|---|
| `bore local <port>` | `--carriers`, `BORE_CARRIERS` | `1` | `1` | same as `1` | server <-> local provider | Yes | Public tunnel mode |
| `bore local <port> --tcp-secret-id <id>` | `--carriers`, `BORE_CARRIERS` | `1` | `1` | same as `1` | server <-> secret provider | Yes | Secret provider mode |
| `bore proxy` | `--carriers`, `BORE_CARRIERS` | `1` | `1` | same as `1` | consumer <-> server | No | Client-side pool only |
| `bore vhost` | `--carriers`, `BORE_CARRIERS` | `1` | `1` | same as `1` | server <-> vhost provider relay / fallback | Yes | If `--udp` is healthy, relay carriers are mostly idle |
| `bore transfer listener` | `--carriers`, `BORE_CARRIERS` | `0` | `min(max(cpu_cores, 4), 16)` | auto | server <-> listener/provider on relay fallback | Yes | Uses the listener CPU hint because it cannot see sender `--parallel` yet |
| `bore transfer sender` | `--carriers`, `BORE_CARRIERS` | `0` | `min(resolve_parallel(--parallel), 16)` | auto | sender/consumer <-> server on relay fallback | No | Direct UDP ignores it |
| `bore server` | `--max-carriers`, `BORE_MAX_CARRIERS` | `16` | `16` | `0` behaves like `1` after clamp | caps public / secret-provider / vhost-provider pools | n/a | Does not cap `bore proxy` |

`resolve_parallel(--parallel)` means:

- if `--parallel 0`, use `available_parallelism()` clamped to `[4, 32]`
- otherwise clamp the explicit value to `[1, 32]`

So the default effective transfer carrier count is usually between `4` and `16`,
depending on CPU count.

## What increasing `--carriers` actually changes

When `N` grows from `1` to `N > 1`:

- the application opens more long-lived TCP connections to the server
- new proxied connections are spread across those carriers round-robin
- each carrier has its own congestion window
- packet loss or backpressure on one carrier stops hurting every other carrier
- aggregate relay throughput can rise under concurrency
- file descriptor usage, TCP state, TLS sessions, keepalives, and memory usage also rise

What does not change:

- one proxied connection still rides one carrier for its lifetime
- a single bulk flow is not split across carriers
- direct UDP / QUIC paths do not become faster because of `--carriers`

The practical ceiling is usually:

`effective gain ~= min(simultaneously busy relay connections, carrier count, server cap where applicable)`

Once carrier count is above the number of busy relay flows, more carriers mostly
add overhead.

## Command-by-command tuning

### `bore local` public tunnel

This is the original public port-forwarding mode. The server opens one proxied
substream per inbound public connection, and `--carriers` chooses how many TCP
connections the client offers for that relay path.

Behavior by value:

- `1`: original single TCP connection; all relayed traffic shares one carrier.
- `2` to `4`: often enough for moderate concurrency, parallel downloads, or many
  independent requests.
- `8` and above: useful only if many relay connections are busy at once and the
  server cap allows it.
- `> --max-carriers`: silently truncated by the server.

For maximum performance:

- Raise `--carriers` only when many relay connections are active at the same time.
- Match carrier count to real concurrency, not peak wishful thinking.
- Keep `1` for single-flow workloads, debugging, or low-resource deployments.

### `bore local --tcp-secret-id ...` secret provider

This is the provider side of a secret tunnel. Here `--carriers` widens the
server-to-provider relay leg. The server round-robins consumer-originated substreams
across the provider carrier pool.

Behavior by value:

- `1`: provider relay path is single-connection.
- `N > 1`: provider opens extra carrier connections after the server issues a
  `CarrierToken`; the server uses that pool for relayed consumer traffic.
- `> --max-carriers`: truncated by the server.

For maximum performance:

- Increase provider carriers when many consumers, or many relay worker streams,
  are feeding the same provider concurrently.
- On relay transfers, tune both provider-side carriers and consumer-side carriers;
  widening only one leg leaves the other leg multiplexed.
- If secret `--udp` succeeds, direct QUIC streams bypass the relay path and the
  carrier pool matters only for fallback traffic.

### `bore proxy`

This is the secret consumer side. Unlike provider/public/vhost pools, the proxy
does not ask the server for a tokenized pool. It simply opens more `ConnectSecret`
connections itself and round-robins forwarded streams across them locally.

That has two important consequences:

- `bore server --max-carriers` does not limit `bore proxy`
- the relay pool is consumer-side only; the server sees multiple normal consumer
  registrations, not one capped server-managed pool

Behavior by value:

- `1`: one consumer relay connection.
- `N > 1`: consumer opens `N` relay connections to the server and spreads local
  forwarded connections across them.
- if direct secret UDP is active, `--carriers` is ignored for steady-state traffic

For maximum performance:

- Raise `--carriers` when one consumer instance is forwarding many concurrent
  relay connections.
- Do not expect benefits on a stable direct UDP path; use `--parallel` on the
  transfer sender instead when the data path is QUIC.
- Be more conservative than on provider-side tuning if you must limit client FD
  usage, because this path is not server-capped.

### `bore vhost`

`bore vhost` uses carriers on the TCP relay between the server and the provider.
If `--udp` is not enabled, or if vhost UDP is unavailable, that relay path is the
steady-state data path. If vhost UDP is healthy, the server opens native QUIC
streams instead and the carrier pool becomes a fallback path.

Behavior by value:

- `1`: single relay carrier.
- `N > 1`: more server-to-provider relay carriers.
- `> --max-carriers`: truncated by the server.

For maximum performance:

- Relay-only vhost: size carriers to the number of simultaneously busy browser
  connections you expect the provider hop to carry.
- Vhost `--udp`: prioritize the UDP path first; keep carriers above `1` only if
  you want better fallback behavior during UDP loss, restart, or disabled-UDP periods.
- Do not expect `--carriers` to improve the browser-to-server leg; it only widens
  the server-to-provider relay leg.

### `bore transfer listener`

The listener is a secret provider under the hood, so its carriers apply only to
the relay fallback path from server to listener. The direct UDP transfer path does
not use them.

Default behavior:

- CLI default is `0`, which means auto.
- Auto resolves to `default_parallel_hint()`.
- `default_parallel_hint()` is `available_parallelism()` clamped to `[4, 32]`.
- Carrier auto then applies a second clamp to `[1, 16]`.

So the default listener carrier count is effectively:

- `4` on small machines with fewer than 4 cores
- `cpu_cores` on machines between 4 and 16 cores
- `16` on machines above 16 cores

For maximum performance:

- Leave it at `0` in most cases.
- Use explicit `1` only when you want the old single-connection path or must keep
  resource usage minimal.
- If relay is forced or frequently used, ensure the listener does not expose fewer
  carriers than the sender's busy worker count, otherwise the provider leg becomes
  the bottleneck.

### `bore transfer sender`

The sender is a secret consumer under the hood, so its carriers apply only to the
consumer-to-server relay fallback path. On the direct UDP path the sender already
gets one QUIC bidi stream per worker connection, so `--carriers` does nothing.

Default behavior:

- CLI default is `0`, which means auto.
- Auto resolves against the sender's resolved `--parallel` value.
- If `--parallel 0`, the sender first resolves parallelism to the same CPU-based
  hint used by the listener, clamped to `[4, 32]`.
- Carrier auto then clamps that to `[1, 16]`.

This makes transfer special: the best default is usually already selected.

For maximum performance:

- Relay path: keep `--carriers 0` and tune `--parallel` first.
- Direct UDP path: tune `--parallel`; `--carriers` is ignored.
- If you force `--carriers 1` while using high `--parallel`, the sender warns
  because many worker streams will be multiplexed over one relay TCP connection.
- If you use explicit carriers, keep them at least as high as the number of busy
  relay workers you expect, or leave auto enabled.

### `bore server --max-carriers`

This is the server-side safety rail, not a performance knob for `bore proxy`.

Behavior by value:

- `1`: disables server-managed pools for public tunnels, secret providers, and
  vhost providers.
- `16`: current default and the upper bound transfer auto is tuned around.
- `0`: effectively behaves like `1`, because server-side clamp sites use
  `max(1)` before applying the limit.
- `N > 16`: allowed, but transfer auto still requests at most `16`; only explicit
  client/provider values can use the larger cap.

For maximum performance:

- Keep `16` unless you have a concrete reason to lower or raise it.
- Lower it when you want tighter FD / connection budgets per tunnel.
- Raise it only when you have measured that more than 16 busy relay flows per
  tunnel are common and the host can afford the extra sockets.
- Remember that it does not constrain `bore proxy`; control proxy fan-out at the
  client side.

## Maximum-performance playbook

### Public tunnel (`bore local` without `--tcp-secret-id`)

- Single stream: keep `--carriers 1`.
- Many concurrent relay connections: try `4`, then `8`, then stop when gains flatten.
- If the server cap is below your request, raise `bore server --max-carriers` or
  accept the clamp.

### Secret tunnel, relay path

- Tune both ends.
- Provider-side (`bore local --tcp-secret-id`) widens server -> provider.
- Consumer-side (`bore proxy`) widens consumer -> server.
- If only one side is wide, the narrower side is still the bottleneck.

### Transfer over relay

- Best default: leave both sender and listener at `--carriers 0`.
- Increase `--parallel` first.
- If you force explicit carrier counts, keep them aligned with real worker
  parallelism on both ends.
- If you want the old behavior for reproducibility or constrained hosts, set
  `--carriers 1` explicitly on both sender and listener.

### Transfer or secret tunnel over direct UDP

- Ignore `--carriers`; it is not the knob that matters.
- Use `--parallel` for transfer throughput.
- Use NAT / STUN / UDP reachability tuning for path establishment quality.

### Vhost

- Stable vhost UDP: `--carriers` is mostly fallback insurance.
- Relay-only vhost: size carriers for concurrent browser connections on the
  server -> provider hop.

## Operational caveats

- Public tunnels, secret providers, transfer listeners, and vhost providers all
  use the shared server-issued carrier-token mechanism.
- Public tunnels and provider-style clients keep trying to top their pools back up
  if an extra carrier drops.
- `bore proxy` currently builds its relay carrier pool client-side and prunes dead
  carriers if they drop; it is not governed by the server token/cap path.
- Transfer auto is intentionally aligned with `DEFAULT_MAX_CARRIERS = 16`, so the
  auto request is not silently truncated by the server default.

## Tests and code paths worth reading

- `tests/carrier_test.rs`: public tunnel carrier pool behavior and server cap
- `tests/secret_pool_test.rs`: secret provider pool, secret consumer pool, relay vs direct behavior
- `tests/transfer_test.rs`: `transfer_auto_carriers_over_relay`
- `src/pool.rs`: round-robin carrier pool implementation
- `src/transfer.rs`: auto carrier resolution for transfer
- `src/secret.rs`: consumer-side relay carrier pool behavior
- `src/server.rs`: server-side clamp and token issuance
