# bore VPN — Android Manual Acceptance

Manual, physical-hardware acceptance procedure for `bore` on Android, per
`docs/plans/plan_AndroidSupport/phase_05.md` §5.2 (mirrors
`VPN_MACOS_ACCEPTANCE.md`'s structure). CI (`android-emu-e2e`, `android-vpn-e2e`)
already validates everything single-host-emulator-reachable; this doc covers what
CI cannot: a real phone, a real network, and real longevity.

**Scope:** Android VPN is **host-only** (D-A4/D-A6/D-A9 — no `--advertise`,
`--nat-masquerade`, `--forward-accept`, or `--max-clients N>1`; see
[Limits and unsupported features](limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md)).
There is no multi-host LAN gateway scenario to test on Android — every test below
is a single Android device talking to one other host (a Linux/macOS box, or the
bore relay server) directly.

Run tests in order; each assumes the previous one's environment is torn down
cleanly unless noted otherwise. Flag names below are verified against
`--help` output of a binary built with `cargo build --features vpn` on
2026-07-03 — if a flag name has since changed, `bore <subcommand> --help`
on-device is the source of truth, not this doc.

## Prerequisites

- An Android device or tablet, Termux installed (F-Droid build, not the
  deprecated Play Store build), API 24+.
- A second machine reachable from the phone's network (a Linux box is assumed
  below; macOS/Windows work identically for the non-VPN tests).
- A `bore server` reachable from both sides (either the project's public demo
  server, or your own — see `docs/ANDROID.md`'s "Root VPN quickstart").
- For T-AND-M3/M4/M5 (root tests): root via Magisk or KernelSU, plus `tsu`
  (`pkg install tsu`).
- For T-AND-M5: the Termux:API add-on (`pkg install termux-api`, plus the
  companion Termux:API app from F-Droid) so `termux-wake-lock` works.

Placeholders used below: `<server>` = `host:port` or `https://host` of your
bore server, `<secret>` = a shared secret string, `<link-id>` = a VPN link
identifier string, `<phone-ip>` = the Android device's LAN or overlay IP as
context requires.

---

## T-AND-M1 — Non-root public tunnel (Termux)

**Preconditions:** Termux installed, `bore` binary present and executable, no
root needed.

**Commands (Android/Termux):**

```bash
# serve a page from the phone itself
python -m http.server 8080 &
./bore local 8080 --to <server>
```

**Expected observation:** `bore local` prints a line containing
`listening at <server-host>:<assigned-port>` (exact wording may vary slightly
by version; the key facts are the assigned public port and confirmation the
control connection is established).

**Commands (second machine):**

```bash
curl -sS http://<server-host>:<assigned-port>/
```

**Expected observation:** the `http.server`'s default directory listing HTML
is returned — proof traffic round-tripped phone → bore server → requesting
machine.

- [ ] PASS

---

## T-AND-M2 — Non-root file transfer (Termux)

**Preconditions:** Same as T-AND-M1, no root. Run phone→PC first, then PC→phone.

### Phone → PC

**Commands (PC, start listener first):**

```bash
bore transfer listener --dest-path ./received --to <server> --transfer-id <link-id>
```

**Commands (Android/Termux, sender):**

```bash
echo "hello from android $(date +%s)" > /data/data/com.termux/files/home/testfile.txt
./bore transfer sender --sources /data/data/com.termux/files/home/testfile.txt \
  --to <server> --transfer-id <link-id>
```

**Expected observation:** sender prints a per-file progress line and exits `0`;
listener prints a completion line and also exits `0` (unless `--persistent`).
`./received/testfile.txt` exists on the PC with matching content. Both sides
run a BLAKE3 verify internally — a mismatch would be a non-zero exit with an
explicit checksum-failure message, not a silent corruption.

```bash
echo $?   # on both sides — must print 0
diff /data/data/com.termux/files/home/testfile.txt ./received/testfile.txt   # PC side, must be empty
```

### PC → Phone

Repeat with sender/listener roles swapped (listener on Android with
`--dest-path`, sender on PC with `--sources`). Same pass criteria.

### Resume test

**Commands:** Start a transfer of a larger file (e.g. `dd if=/dev/urandom
of=bigfile.bin bs=1M count=200`) from PC to phone. Mid-transfer, `Ctrl-C` the
**listener** (Android side) partway through, then immediately rerun the exact
same `bore transfer listener ...` command.

**Expected observation:** the rerun listener resumes from where it left off
(does not restart from byte 0 — visible as a much shorter remaining-time/byte
count than a fresh transfer of the same file) and completes with exit `0`,
BLAKE3-verified.

- [ ] PASS

---

## T-AND-M3 — Root VPN host-only connect + clean teardown

**Preconditions:** Root (Magisk/KernelSU) + `tsu` installed. A Linux box
running `bore server` and available to run `bore vpn listen`.

**Commands (Linux box):**

```bash
bore server &
bore vpn listen --to <server> --secret <secret> --id <link-id>
```

**Commands (Android, as root):**

```bash
tsu
./bore vpn connect --to <server> --secret <secret> --id <link-id>
```

**Expected observation:** both sides log a successful link establishment
(overlay addresses assigned from the server pool, since neither side passed
`--vpn-addr`). `ip addr show bore0` (or `bore1`, etc. — see `pick_tun_name` in
`docs/ANDROID.md`) on the Android side shows the assigned overlay IP.

**Bidirectional ping:**

```bash
# from Android, root shell
ping -c 4 <linux-overlay-ip>

# from the Linux box
ping -c 4 <phone-overlay-ip>
```

**Expected observation:** both directions succeed (0% packet loss). This is
the direct confirmation of the netd routing-policy fix documented in
`docs/ANDROID.md` (`ip rule add to <subnet> lookup main priority 100`) — a
regression there shows up as the *host-initiated* ping direction hanging while
guest-initiated replies work.

**Teardown:**

```bash
# on Android, in the tsu shell running bore vpn connect
<Ctrl-C>
```

**Expected observation:** clean exit; `ip addr show bore0` (or the assigned
name) errors with "does not exist" (interface removed);
`ip route show table main | grep bore` returns nothing (routes reverted);
`ip rule show` still lists the `to <subnet> lookup main priority 100` rule
(this is the documented permanent leak, not a bug — see gap #12 in
`VPN_ANDROID_ACTUAL_LIMIT.md`, expected here, not a failure).

- [ ] PASS

---

## T-AND-M4 — SIGKILL reclaim (no-op, by design)

**Preconditions:** Same setup as T-AND-M3, link re-established.

**Commands (Android, root shell, find the PID and SIGKILL it):**

```bash
pgrep -f "bore vpn connect"
kill -9 <pid>
```

**Expected observation:** the process disappears with no teardown at all (no
RAII on SIGKILL — the TUN device and `ip rule`/route state are left behind
exactly as documented in `docs/ANDROID.md`'s "No RAII state files on Android"
section). This is expected, not a failure.

**Relaunch:**

```bash
./bore vpn connect --to <server> --secret <secret> --id <link-id>
```

**Expected observation:** the relaunch succeeds and establishes a fresh link
(possibly with a new `pick_tun_name`-assigned interface, e.g. `bore1` if
`bore0` is still lingering from the killed process). **Do not expect a
`stale_reclaim` log line** — Android's `apply()` never writes an
`.ipforward`/`.fwdref` marker in the first place (host-only, no `ip_forward`
ever touched), so `stale_reclaim` always finds zero files and logs nothing;
this is a verified no-op on every platform's marker scheme, not something
broken on Android. The correct assertion is a **clean process start** with no
error about a stale state file, not the presence of a log line.

```bash
ls /data/local/tmp/ | grep -E '\.ipforward|\.fwdref'   # must print nothing, before and after
```

- [ ] PASS

---

## T-AND-M5 — Longevity under the phantom-process killer

**Preconditions:** Root, `termux-wake-lock` available (Termux:API app +
`pkg install termux-api`). A link established per T-AND-M3.

**Commands:**

```bash
termux-wake-lock
./bore vpn connect --to <server> --secret <secret> --id <link-id> --auto-reconnect &
```

Lock the phone screen (power button) and leave it for 30 minutes.

**Expected observation:** after 30 minutes, unlock the phone and confirm the
link is still alive:

```bash
ping -c 4 <linux-overlay-ip>
```

Succeeds with 0% loss, and the process is still running
(`pgrep -f "bore vpn connect"` still returns the same PID — no restart via
`--auto-reconnect` was needed, i.e. the phantom-process killer did not fire).

**If the process was killed** (PID gone or changed, ping requires a moment to
recover via `--auto-reconnect`'s reconnect logic): the `termux-wake-lock`
remediation alone was insufficient on this device/Android version. Apply the
stronger remediation and repeat:

```bash
adb shell device_config put activity_manager max_phantom_processes 2147483647
```

(or the blunter `adb shell settings put global
settings_enable_monitor_phantom_procs false`).

**Record for this run:**
- Observed Android version: ____________________
- Was `termux-wake-lock` alone sufficient? Y / N
- If N, was the `device_config`/`settings` step required to pass? Y / N

- [ ] PASS

---

## Summary

| Test | Covers | Root required |
|------|--------|---|
| T-AND-M1 | Non-root public tunnel, phone as server | No |
| T-AND-M2 | Non-root file transfer both directions + resume | No |
| T-AND-M3 | Root VPN host-only connect, bidirectional ping, clean SIGINT teardown | Yes |
| T-AND-M4 | SIGKILL + relaunch, verifying the documented `stale_reclaim` no-op | Yes |
| T-AND-M5 | 30-minute screen-off longevity vs the phantom-process killer | Yes |

All five PASS ⇒ Android VPN + non-VPN subcommands are accepted for real-world
use. Any FAIL should be filed with the exact Android version, device model,
root method (Magisk/KernelSU), and Termux version — these are the axes most
likely to explain a platform-specific deviation from CI's emulator behavior.

## See also

- [docs/ANDROID.md](../ANDROID.md) — install, feature matrix, CLI guard matrix, VPN backend reference
- [Limits and unsupported features](limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md)
- [plan_AndroidSupport](../plans/plan_AndroidSupport/) — full project plan + status + resume
