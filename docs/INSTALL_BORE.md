# bore install — usage & customization

## Quick install

Default path `/home/$USER/.bin`:

```bash
curl -fsSL https://short.sish.adiprint.it/install-bore | bash
```

Custom path via env var:

```bash
curl -fsSL https://short.sish.adiprint.it/install-bore | INSTALL_PATH=/home/$USER/.bin bash
```

or

```bash
curl -fsSL https://short.sish.adiprint.it/install-bore | INSTALL_PATH=/usr/local/bin sudo bash
```

## Source — direct (no file saved)

Run script in current shell via process substitution. No file written to disk.

Default path:

```bash
source <(curl -fsSL https://short.sish.adiprint.it/install-bore)
```

Custom path with sudo:

```bash
INSTALL_PATH=/home/$USER/.bin bash -c 'source <(curl -fsSL https://short.sish.adiprint.it/install-bore)'
```

or 

```bash
INSTALL_PATH=/usr/local/bin sudo -E bash -c 'source <(curl -fsSL https://short.sish.adiprint.it/install-bore)'
```

Or step-by-step (download → inspect → run):

```bash
curl -fsSL -o install-bore.sh https://short.sish.adiprint.it/install-bore
chmod +x install-bore.sh
# inspect/edit vars if needed
./install-bore.sh
```

With custom path:

```bash
INSTALL_PATH=/home/$USER/.bin ./install-bore.sh
```

## Env vars

| Var | Default | Effect |
|-----|---------|--------|
| `INSTALL_PATH` | `/usr/local/bin` | Where `binary` lands |

## What script does

1. Detect OS (linux/macos/android) and arch (amd64/arm64)
2. Pick URL from `BORE_*` vars
3. Download with `curl` (fallback: `wget`)
4. Verify binary via `--version`
5. Move to `$INSTALL_PATH/bore` (sudo if no write perms)

## Update bore

Re-run same script. Overwrites old binary.

## Uninstall

```bash
sudo rm /usr/local/bin/bore
# or: sudo rm $INSTALL_PATH/bore
```

## Windows

The install script above is bash-only (Linux/macOS/Android); Windows install
is manual:

1. Download `bore-<version>-x86_64-pc-windows-msvc.zip` (or `i686-...` for
   32-bit) from the [releases page](https://github.com/manprint/bore/releases).
2. Unzip. The archive contains `bore.exe` and `wintun.dll` side by side —
   both need to stay in the same folder (WinTun's default DLL search checks
   the executable's own directory first).
3. Move the folder somewhere on `%PATH%`, or run `bore.exe` from inside it.
4. Verify: `bore.exe --version`.

### WinTun prerequisite (`bore vpn` only)

Every non-VPN subcommand (`local`, `proxy`, `server`, `vhost`, `transfer`,
`test-udp`) needs nothing beyond `bore.exe` itself. `bore vpn listen`/
`connect` additionally needs:

- **`wintun.dll`** next to `bore.exe` (already bundled in the release zip —
  see [docs/vpn/VPN_WINDOWS.md](vpn/VPN_WINDOWS.md) for the pinned version/
  hash and the redistribution rationale) or pointed to via the
  `BORE_WINTUN_DLL` environment variable if you keep it elsewhere.
- **An elevated (Administrator) shell.** Right-click PowerShell/cmd → "Run as
  administrator", or launch from an already-elevated terminal. `bore vpn`
  fails immediately (before creating any adapter) if the process token isn't
  elevated.

If you build from source instead of using the release zip, `wintun.dll`
isn't produced by `cargo build` — fetch it yourself (official signed DLL
only, per WinTun's own distribution terms) with:

```powershell
./scripts/fetch_wintun.ps1 -Arch amd64 -OutFile target/release/wintun.dll
```
