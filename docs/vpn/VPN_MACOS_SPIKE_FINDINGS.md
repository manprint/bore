# bore vpn macOS spike findings

**Test ID:** Phase 1, Sub-phase 1.2 (de-risk spike on real macOS)

Recorded results from running `examples/macos_vpn_spike.rs` as root on macOS 13+.

---

## How to run

Build the spike binary on macOS (Apple Silicon, with Rust installed):

```bash
cargo build --features vpn --example macos_vpn_spike
sudo target/debug/examples/macos_vpn_spike spike
```

The spike validates PF grammar and utun behavior before any runtime integration. It creates a utun,
exercises the `cmd_*` builders and the `pf_ruleset` composer, loads rules with `pfctl`, and verifies
RAII teardown.

---

## Test environment

| Field | Value |
|---|---|
| macOS version | TODO: fill on Mac |
| Hardware | TODO: fill on Mac (e.g., Apple Silicon M1/M2/M3) |
| Test date | TODO: fill on Mac |
| Operator | TODO: fill on Mac |

---

## utun naming and behavior

**Does a requested utun name persist, or does the kernel assign one (e.g., always `utunN` in sequence)?**

```
TODO: fill on Mac
Example observed: utun4, utun5, etc.
```

---

## pfctl grammar validation

Run the spike; it loads a sample ruleset via `pfctl -a bore_vpn/spike0 -f <tmp>` and captures stderr.
Record every line exactly, including all rejected lines with their stderr.

### Accepted rules

```
TODO: fill on Mac

pfctl loading accepted without error.
```

### Rejected rules (if any)

```
TODO: fill on Mac

If pfctl rejected any rule (exit status != 0), paste the exact stderr here.
Example:
  pfctl: unknown operator foo
```

---

## sysctl net.inet.ip.forwarding — read and write

**Does `sysctl net.inet.ip.forwarding` succeed as root to both read and write?**

```
TODO: fill on Mac

Example:
  $ sudo sysctl net.inet.ip.forwarding
  net.inet.ip.forwarding: 0
  $ sudo sysctl -w net.inet.ip.forwarding=1
  net.inet.ip.forwarding: 0 -> 1
  $ sudo sysctl net.inet.ip.forwarding
  net.inet.ip.forwarding: 1
```

---

## route -n get output format vs parse_lan_iface expectation

The spike runs the builders from `hostcfg_cmd::macos`. One of them is `cmd_route_get`, which returns
the output of `route -n get <host>` and feeds it to `parse_lan_iface` to extract the LAN interface.

**Exact `route -n get <some-ip>` output:**

```
TODO: fill on Mac

Example:
   route -n get 8.8.8.8
   ... (all lines)
   interface: en0
   ... (rest)
```

**Does `parse_lan_iface` extract the correct interface from the output above?** (Yes/No, describe any issue)

```
TODO: fill on Mac
```

---

## Builder and ruleset corrections

Did `pfctl` reject any rules, or did `parse_lan_iface` mismatched the output? If so, record the exact
correction needed in `src/vpn.rs::hostcfg_cmd::macos` or the `pf_ruleset` function.

**Example issue and fix:**

```
TODO: fill on Mac if needed

If no issues, state: "No corrections needed."
```

---

## Opus sign-off

Gate Phase 2 on Opus review of the findings above. Once validated, Opus will:

- [ ] Confirm PF grammar is correct (or record the patched `pf_ruleset` and `cmd_*` implementations).
- [ ] Confirm `parse_lan_iface` matches the route output (or direct the fix).
- [ ] Update snapshot tests `cmd_macos_builders_snapshot`, `macos_pf_ruleset_*` in `src/vpn.rs` to reflect
  the validated argv/ruleset.
- [ ] Replace "PROVISIONAL" wording in `pf_ruleset` doc comment (`src/vpn.rs:3069–3105`) and the
  `hostcfg_cmd::macos` header (`src/vpn.rs:2905`) with "validated on macOS <version>".
- [ ] Confirm Linux snapshot tests remain green after any patches.

**Opus approval:** ☐ Findings reviewed and grammar/builders confirmed or corrected.

---

## Notes

Attach any additional observations (e.g., device creation timing, pfctl performance, kernel warnings).

```
TODO: fill on Mac if relevant
```
