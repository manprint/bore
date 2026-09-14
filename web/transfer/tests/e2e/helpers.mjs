// Shared Playwright setup for the web-transfer room suites: a real `bore
// server` with web-transfer on a dynamic loopback port plus one room owner
// holding its lease. Both binaries come from a prior
// `cargo test --all-features` / `cargo build --all-features`.
import { spawn } from "node:child_process";
import net from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "..");
export const boreBin = join(root, "target", "debug", "bore");
export const ownerBin = join(root, "target", "debug", "examples", "web_transfer_e2e_owner");

export function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

export async function waitPort(target, retries = 100) {
  for (let i = 0; i < retries; i += 1) {
    const open = await new Promise((resolve) => {
      const socket = net.connect(target, "127.0.0.1");
      socket.once("connect", () => {
        socket.end();
        resolve(true);
      });
      socket.once("error", () => resolve(false));
    });
    if (open) {
      return;
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`port ${target} never opened`);
}

/**
 * Starts a server plus one lease-holding owner. Returns
 * `{ port, roomUrl, roomId, memberToken, roomKey, cleanup }`.
 */
export async function spawnRoomEnv() {
  const port = await freePort();
  const server = spawn(
    boreBin,
    ["server", "--control-port", String(port), "--web-transfer-base-url", `http://127.0.0.1:${port}/`],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  server.on("error", (error) => {
    throw new Error(`cannot spawn ${boreBin}: ${error.message} (run cargo build --all-features first)`);
  });
  await waitPort(port);
  // Freshness: the server embeds dist at compile time; a stale binary
  // serves a shell with no room logic and every test times out.
  const asset = await fetch(`http://127.0.0.1:${port}/transfer/assets/app.js`, {
    headers: { Host: `127.0.0.1:${port}` },
  }).then((response) => response.text());
  if (!asset.includes("peer-list")) {
    server.kill("SIGKILL");
    throw new Error("stale embedded app bundle: run cargo build --all-features after npm run build");
  }
  const owner = spawn(ownerBin, [`127.0.0.1:${port}`], { stdio: ["ignore", "pipe", "pipe"] });
  owner.on("error", (error) => {
    throw new Error(`cannot spawn ${ownerBin}: ${error.message} (run cargo build --all-features first)`);
  });
  let buffered = "";
  const roomUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("owner printed no room URL in time")), 30_000);
    owner.stdout.on("data", (chunk) => {
      buffered += String(chunk);
      const line = buffered.split("\n").find((candidate) => candidate.startsWith("WEB_TRANSFER_ROOM_URL="));
      if (line !== undefined) {
        clearTimeout(timer);
        resolve(line.slice("WEB_TRANSFER_ROOM_URL=".length).trim());
      }
    });
    owner.stdout.on("error", reject);
  });
  const parsed = new URL(roomUrl);
  const roomId = parsed.pathname.split("/").pop();
  const memberToken = parsed.hash.match(/m=([0-9a-f]{64})/)?.[1];
  const roomKey = parsed.hash.match(/k=([0-9a-f]{64})/)?.[1];
  if (!roomId || !memberToken || !roomKey) {
    server.kill("SIGKILL");
    owner.kill("SIGKILL");
    throw new Error(`unparseable room URL ${roomUrl}`);
  }
  return {
    port,
    roomUrl,
    roomId,
    memberToken,
    roomKey,
    cleanup: () => {
      owner.kill("SIGKILL");
      server.kill("SIGKILL");
    },
  };
}

/** Canonical JSON (sorted keys, no whitespace) for the MAC cross-check. */
export function canonicalize(value) {
  if (value === null) return "null";
  if (value === true) return "true";
  if (value === false) return "false";
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) {
      throw new Error("no canonical form");
    }
    return String(value);
  }
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) {
    return `[${value.map(canonicalize).join(",")}]`;
  }
  if (typeof value === "object") {
    const keys = Object.keys(value).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalize(value[k])}`).join(",")}}`;
  }
  throw new Error("no canonical form");
}
