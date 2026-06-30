# bore VPN on Windows

> Status: implementation in progress. This document tracks the Windows backend decisions and operator requirements while `docs/plans/plan_WindowsSupport/` is executed.

## D-WT1: WinTun backend

Windows VPN uses WinTun as the L3 packet device backend.

Rationale:
- bore VPN is an L3 IPv4 packet tunnel; WinTun is a Windows L3 TUN driver.
- TAP/L2 drivers are not the default path because they add Ethernet framing and a heavier driver/install model.
- The upstream WinTun distribution model is `wintun.dll`; bore loads it at runtime.
- The Rust bindings expose safe adapter/session operations after a required unsafe DLL load boundary.
- bore's main crate uses `#![forbid(unsafe_code)]`; therefore unsafe DLL loading is isolated in the local `bore-wintun` wrapper crate.

Chosen Rust binding:
- Crate: `wintun-bindings` 0.7.39.
- License: MIT.
- Bore-facing wrapper: `crates/bore-wintun`.

API facts used by the implementation:
- `wintun_bindings::load()` loads `wintun.dll` from the default DLL search path.
- `wintun_bindings::load_from_path(...)` loads an explicit DLL path.
- `Adapter::create(...)` and `Adapter::open(...)` create/open adapters.
- `Adapter::start_session(...)` starts a packet session.
- `Session::recv(...)`, `Session::send(...)`, and `Session::shutdown(...)` cover packet I/O and shutdown.

Operational policy:
- `wintun.dll` should be placed next to `bore.exe` or provided through `BORE_WINTUN_DLL`.
- `BORE_WINTUN_DLL` must point to a trusted DLL path.
- Missing DLL must fail before host networking side effects.
- Windows VPN requires an elevated shell for adapter creation and host networking changes.

## Current limitations

The Windows backend is not complete yet. Current code exposes the VPN CLI on Windows and returns an explicit unsupported-backend error until WinTun TUN I/O and Windows host networking apply/revert are complete.

Required before support can be declared complete:
- Real WinTun adapter creation/read/write integration.
- Windows route/IP forwarding/firewall/NAT apply and revert.
- Overlapping-subnet `real@virtual` 1:1 prefix translation backend.
- Elevated Windows e2e tests.
- Cross-OS VPN relay/direct/hub/NAT acceptance.
