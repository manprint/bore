# VHOST Test Matrix

All integration tests live in `tests/vhost_test.rs`. Unit tests are in `src/vhost.rs` (`#[cfg(test)]`).

---

## Unit tests (`src/vhost.rs`)

| Test | Scenario | Status |
|---|---|---|
| `extract_subdomain_basic` | `mysub.bore.example.com` → `"mysub"` | ✅ |
| `extract_subdomain_strips_port` | Host with `:443` suffix → strips port | ✅ |
| `extract_subdomain_case_insensitive` | `MySub.Bore.Example.Com` → `"mysub"` | ✅ |
| `extract_subdomain_wrong_base_domain` | Different base domain → `None` | ✅ |
| `extract_subdomain_nested_label` | `a.b.bore.example.com` → `None` | ✅ |
| `extract_subdomain_empty_label` | `bore.example.com` (no label) → `None` | ✅ |
| `extract_subdomain_illegal_chars` | `my_sub.bore...` → `None` | ✅ |
| `extract_subdomain_leading_hyphen` | `-bad.bore...` → `None` | ✅ |
| `extract_subdomain_trailing_hyphen` | `bad-.bore...` → `None` | ✅ |
| `parse_config_round_trips` | Representative yaml round-trips into struct | ✅ |
| `parse_config_defaults` | Missing optional fields use defaults | ✅ |
| `parse_config_unknown_mode_errors` | Unknown `mode` value → `Err` | ✅ |
| `resolve_route_reserved_matching_accepts` | Reserved subdomain, correct id → Accept | ✅ |
| `resolve_route_reserved_other_id_rejects` | Reserved subdomain, wrong id → Reject | ✅ |
| `resolve_route_unreserved_accepts` | Unlisted subdomain → Accept | ✅ |
| `merge_headers_override` | Per-subdomain key overrides default | ✅ |
| `merge_headers_disjoint_union` | Disjoint keys both present | ✅ |
| `resolve_mode_no_cert_forces_http` | No cert → `VhostMode::Http` | ✅ |
| `resolve_mode_https_no_cert_errors` | `https` mode, no cert → `Err` | ✅ |
| `resolve_mode_both_no_cert_errors` | `both` mode, no cert → `Err` | ✅ |
| `resolve_mode_redirect_https_no_cert_errors` | `redirect-https`, no cert → `Err` | ✅ |
| `resolve_mode_auto_with_cert_returns_both` | `auto` + cert → `VhostMode::Both` | ✅ |
| `public_urls_http_default_port_no_suffix` | Port 80 → no `:80` in URL | ✅ |
| `public_urls_non_default_ports_include_port` | Non-default ports → include in URL | ✅ |
| `public_urls_redirect_mode_no_http_url` | `redirect-https` → no HTTP URL | ✅ |
| `hello_vhost_round_trips_and_fits_frame` | `ClientMessage::HelloVhost` serialises ≤ frame limit | ✅ |
| `vhost_ready_round_trips` | `ServerMessage::VhostReady` round-trips | ✅ |
| `rewrite_head_*` | Head-rewrite pure function coverage | ✅ |

---

## Integration tests (`tests/vhost_test.rs`)

### Registration

| Test | Scenario | Status |
|---|---|---|
| `vhost_provider_registers` | Client registers, receives VhostReady | ✅ |
| `vhost_duplicate_subdomain_rejected` | Second client on same subdomain → Err | ✅ |
| `vhost_reservation_enforced_accepted` | Reserved subdomain, matching id → Ok | ✅ |
| `vhost_reservation_enforced_rejected` | Reserved subdomain, wrong id → Err | ✅ |
| `vhost_subdomain_freed_after_disconnect` | Re-register same subdomain within 500 ms | ✅ |

### HTTP routing

| Test | Scenario | Status |
|---|---|---|
| `vhost_http_routing` | GET via Host header → correct body from stub | ✅ |
| `vhost_unknown_subdomain_502` | No provider → 502, completes within 3 s | ✅ |
| `vhost_header_injection` | Default + per-subdomain headers merged, arrive at stub | ✅ |
| `vhost_large_body_integrity` | 1 MiB response, byte-exact (half-close correctness) | ✅ |
| `vhost_concurrency_smoke` | 5 subdomains × concurrent requests, no cross-talk | ✅ |

### HTTPS routing

| Test | Scenario | Status |
|---|---|---|
| `vhost_https_routing` | TLS terminated, Host header routed, body correct | ✅ |
| `vhost_redirect_mode` | Plain HTTP → 308 with `Location: https://...` | ✅ |

### Hot reload

| Test | Scenario | Status |
|---|---|---|
| `vhost_config_hot_reload` | Update vhost.yml → new reservation rules apply | ✅ |
| `vhost_bad_config_ignored` | Malformed yaml → server keeps old config, no crash | ✅ |
| `vhost_cert_hot_reload` | Swap cert files → new cert served | ⬜ future work |

### CLI and reconnect

| Test | Scenario | Status |
|---|---|---|
| `vhost_auto_reconnect` | Client starts before server, server appears → routing works | ✅ |
| `vhost_https_mode_without_cert_errors` | `set_vhost(Https, no cert)` → `Err` | ✅ |

---

## Coverage gaps / future work

The following scenarios are **not yet tested** or are **explicitly deferred**:

| Gap | Reason / tracking |
|---|---|
| `vhost_cert_hot_reload` | Requires coordinating in-flight TLS stream + cert swap; complex to do deterministically without sleeping for cert TTL. Planned for a follow-up. |
| Per-request header injection on keep-alive | Feature itself is MVP-limited to first request only. Full keep-alive injection is future work. |
| QUIC server↔client relay transport | Not implemented in MVP. |
| Multi-map per `bore vhost` invocation | Not implemented in MVP. |
| SNI-based multi-certificate routing | Not implemented in MVP. |
| Nested subdomain labels (`a.b.bore.…`) | Explicitly rejected. Future: configurable nesting. |
| Per-client distinct secrets | Not implemented in MVP. |
| `--basic-auth` enforcement on vhost connections | Flag is display-only in MVP (carried to admin page). Full enforcement same as secret providers is future work. |
| `--max-conns` exhaustion on vhost frontend | Semaphore logic inherited from server; not separately exercised. |
| Concurrent config reloads (two rapid writes) | Race window is the 2 s poll interval; the reload task is single-threaded so no concurrent writes. |
