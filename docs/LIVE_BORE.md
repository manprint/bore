# bore live - usage & customization

`live-bore.sh` lets you run `bore` straight from `curl` without installing anything on the host. It downloads the right release asset for the current OS/arch, runs the command you pass, and deletes the temporary binary on exit.

## Quick live run

Linux / macOS / Android:

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- local 8000
```

Server mode:

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- server
```

Proxy mode:

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- proxy 8000
```

Windows-like shells (Git Bash, MSYS2, Cygwin):

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- local 8000
```

## What the script does

1. Detects the operating system and architecture.
2. Selects the matching GitHub release asset.
3. Downloads the binary into a temporary directory.
4. Verifies the binary with `--version`.
5. Runs `bore` with the arguments you passed to the script.
6. Removes the temporary directory automatically on exit, even after errors or Ctrl-C.

## Platform mapping

| Detected system | Release asset | Path mode shown by the script |
|------------------|---------------|-------------------------------|
| Linux amd64 | `bore-x86_64-unknown-linux-musl` | POSIX (slash paths) |
| Linux arm64 | `bore-aarch64-unknown-linux-musl` | POSIX (slash paths) |
| macOS amd64 | `bore-x86_64-apple-darwin` | POSIX (slash paths) |
| macOS arm64 | `bore-aarch64-apple-darwin` | POSIX (slash paths) |
| Android arm64 | `bore-aarch64-linux-android` | POSIX (slash paths) |
| Windows amd64 | `bore-x86_64-pc-windows-msvc.exe` | Windows (backslash paths) |
| Windows i686 | `bore-i686-pc-windows-msvc.exe` | Windows (backslash paths) |

## Environment variables

| Var | Default | Effect |
|-----|---------|--------|
| `BORE_RELEASE_BASE` | `https://github.com/manprint/bore/releases/latest/download` | Alternate release mirror or private artifact base |

## Notes

- The script does not add anything to `PATH` and does not touch shell rc files.
- The temporary binary is removed automatically when the command exits.
- Use `bash -s --` in the pipe so the bore arguments are forwarded correctly.

## Examples

Run a local tunnel:

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- local 3000
```

Run the server:

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- server
```

Run a proxy:

```bash
curl -fsSL https://short.sish.adiprint.it/bore | bash -s -- proxy 3000
```