# Transfer Test Matrix

Coverage map for `bore transfer listener` / `bore transfer sender`.
Unit tests live in `src/transfer.rs #[cfg(test)]`; integration tests in `tests/transfer_test.rs`.

---

## 1. Transport paths

| Scenario | Test | Status |
|---|---|---|
| Single file — relay (TLS) | `transfer_single_file_over_relay` | ✅ |
| Single file — relay (insecure) | `transfer_single_file_over_tls_control_with_insecure` | ✅ |
| Single file — direct UDP / QUIC | `transfer_single_file_over_direct_udp` | ✅ |
| Single file — direct UDP, NAT flags | `transfer_single_file_over_direct_udp_with_nat_flags_enabled` | ✅ |
| UDP disabled on listener → fallback relay | `transfer_single_file_falls_back_to_relay_when_listener_disables_direct_udp` | ✅ |
| UDP disabled on sender → fallback relay | `transfer_single_file_falls_back_to_relay_with_nat_flags_enabled` | ✅ |

---

## 2. File content & size edge cases

| Scenario | Test | Status |
|---|---|---|
| Zero-byte file | `transfer_zero_byte_file_over_relay` | ✅ |
| File sizes around chunk boundaries (1, CHUNK-1, CHUNK, CHUNK+1, 2×CHUNK) | `transfer_file_size_boundaries_over_relay` | ✅ |
| Manifest spanning multiple frames | `transfer_manifest_spans_multiple_frames_over_relay` | ✅ |
| Large file, parallel workers (relay) | `transfer_large_file_parallel_over_relay` | ✅ |
| Small file, parallel workers > chunk count | `transfer_small_file_parallel_over_relay_when_workers_exceed_chunks` | ✅ |
| `chunk_count_for` — 0, exact, over, large | unit tests (×4) | ✅ |
| `chunk_len` — first, last partial, zero-byte | unit tests (×3) | ✅ |

---

## 3. Source types & multi-source

| Scenario | Test | Status |
|---|---|---|
| Single file | `transfer_single_file_over_relay` | ✅ |
| Single directory (structure preserved) | `transfer_directory_preserves_structure` | ✅ |
| Multi-source **with** `--output` (named wrapper dir) | `transfer_multi_source_files_over_relay` | ✅ |
| Multi-source **without** `--output` (flat to dest_root) — Bug 002 | `transfer_multi_source_flat_no_output_over_relay` | ✅ |
| Source from `--source-files` list | `transfer_source_files_flag` | ✅ |
| Stdin stream (via `bore transfer sender --source stdin`) | `transfer_stdin_cli_test.rs` | ✅ |
| `scan_multi_filesystem` sets `multi_source=true` without `--output` | unit test | ✅ |
| `scan_multi_filesystem` clears `multi_source=false` with `--output` | unit test | ✅ |

---

## 4. Symbolic links & special files

| Scenario | Test | Status |
|---|---|---|
| Symlinks excluded (default) | `transfer_directory_excludes_symlinks_when_requested` | ✅ |
| Root source is a symlink → rejected when `--symlinks=exclude` | `transfer_root_symlink_is_rejected_when_symlinks_are_excluded` | ✅ |
| Root source is a device → rejected when `--devices=exclude` | `transfer_root_device_is_rejected_when_devices_are_excluded` | ✅ |
| Device transfer with `--devices=include` | `transfer_root_device_honors_devices_include_over_relay` | ✅ |
| Non-UTF-8 file name | `transfer_non_utf8_file_name_over_relay` | ✅ |
| Non-UTF-8 nested directory | `transfer_non_utf8_nested_directory_over_relay` | ✅ |

---

## 5. Collision policies

| Scenario | Test | Status |
|---|---|---|
| Destination exists — `Fail` (default) | `transfer_fail_existing_file_over_relay` | ✅ |
| Destination exists — `Overwrite` | `transfer_overwrite_existing_file_over_relay` | ✅ |
| Destination exists — `Rename` | `transfer_rename_existing_file_over_relay` | ✅ |
| Persistent listener: collision error does not stop listener | `transfer_persistent_listener_collision_continues` | ✅ |

---

## 6. Resume / crash recovery

| Scenario | Test | Status |
|---|---|---|
| Resume large file (relay) | `transfer_resume_large_file_over_relay` | ✅ |
| Resume large file (direct UDP) | `transfer_resume_large_file_over_direct_udp` | ✅ |
| Resume large file (UDP → fallback relay) | `transfer_resume_large_file_over_udp_request_fallback_relay` | ✅ |
| Resume rejected when manifest changed | `transfer_resume_rejects_changed_manifest_over_relay` | ✅ |
| `build_chunk_tasks` skips already-completed chunks | unit test | ✅ |
| `build_chunk_tasks` generates tasks for all chunks | unit test | ✅ |

---

## 7. Persistent listener

| Scenario | Test | Status |
|---|---|---|
| Two sequential transfers, same listener | `transfer_persistent_listener_two_sequential_transfers` | ✅ |
| Collision in persistent mode — listener continues | `transfer_persistent_listener_collision_continues` | ✅ |

---

## 8. Manifest & protocol

| Scenario | Test | Status |
|---|---|---|
| `validate_manifest` — single file ok | unit test | ✅ |
| `validate_manifest` — directory with child ok | unit test | ✅ |
| `validate_manifest` — empty manifest rejected | unit test | ✅ |
| `validate_manifest` — non-empty root rel_path rejected | unit test | ✅ |
| `validate_manifest` — duplicate ids rejected | unit test | ✅ |
| `validate_manifest` — duplicate paths rejected | unit test | ✅ |
| `validate_manifest` — file missing size rejected | unit test | ✅ |
| `validate_manifest` — wrong chunk_count rejected | unit test | ✅ |
| `manifest_hash` is deterministic | unit test | ✅ |
| `manifest_hash` differs for different manifests | unit test | ✅ |
| `summary_from_materialized_entries` counts correctly | unit test | ✅ |
| `summary_from_materialized_entries` is deterministic | unit test | ✅ |

---

## 9. Path encoding

| Scenario | Test | Status |
|---|---|---|
| UTF-8 component round-trip | unit test | ✅ |
| Unix raw bytes (non-UTF-8) round-trip | unit test (unix) | ✅ |
| Windows reserved names sanitized | unit test (windows) | ✅ |
| Windows wide chars round-trip | unit test (windows) | ✅ |
| `encode_relative_path` / `decode_relative_path` round-trip | unit test | ✅ |
| Empty relative path encodes to empty string | unit test | ✅ |

---

## 10. User-experience / error paths (Bug regressions)

| Scenario | Bug | Test | Status |
|---|---|---|---|
| `--ask-confirm` on tty: waits for `y`/`n` | 001 | manual / /dev/tty | ✅ fix |
| `--ask-confirm` on non-tty: returns clear error, not "cancelled" | 001 | `transfer_ask_confirm_returns_err_when_no_tty_available` | ✅ |
| Multi-source without `--output`: flat to dest_root | 002 | `transfer_multi_source_flat_no_output_over_relay` | ✅ |
| Multi-source with `--output`: wrapped in named dir | 002 | `transfer_multi_source_files_over_relay` | ✅ |
| No listener running: helpful error, not raw EOF | 003 | `transfer_sender_fails_with_helpful_message_when_no_listener` | ✅ |
| `fail_existing` sender gets "destination already exists" | regression | `transfer_fail_existing_file_over_relay` | ✅ |

---

## 11. Helpers / utilities

| Scenario | Test | Status |
|---|---|---|
| `human_bytes` formatting | unit test | ✅ |
| `format_duration` short / minutes | unit tests (×2) | ✅ |

---

## Coverage gaps (known)

| Area | Why not covered |
|---|---|
| Windows symlinks | Platform-specific; not in CI |
| Device transfer (char/block) in multi-source | Requires root; covered only for single-source |
| `--ask-confirm` with real terminal + `y`/`n` input | Cannot automate tty input in tests |
| Very large files (> 10 GiB) | Disk / time constraints in CI |
| Network errors mid-transfer | Requires low-level TCP injection |
| `rename_component` with > 9999 existing copies | Extreme edge case |
| `CollisionPolicy::Rename` in multi-source flat mode | Not yet implemented (defaults to Fail) |

---

*Updated: 2026-06-06 — after fixing bugs 001, 002, 003.*
*Test counts: 32 integration + 30 unit = 62 transfer-specific tests.*
