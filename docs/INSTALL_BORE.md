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
