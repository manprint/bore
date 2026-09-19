// How many rtc.ice messages one negotiation really costs, per engine.
// The server's control bucket has to absorb a LEGAL negotiation; sizing it
// needs this number and not an estimate.
import { spawnRoomEnv, openPersistentPeer } from "../e2e/helpers.mjs";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const engine = process.argv[2] ?? "webkit";
const env = await spawnRoomEnv({});
const dir = mkdtempSync(join(tmpdir(), "ice-"));
writeFileSync(join(dir, "piccolo.bin"), Buffer.alloc(2_000_000, 5));

const a = await openPersistentPeer(env.roomUrl, { browserName: engine });
const b = await openPersistentPeer(env.roomUrl, { browserName: engine });
for (const p of [a, b]) {
  await p.page.locator("#room-status").filter({ hasText: "Connesso" }).first().waitFor({ timeout: 20000 });
}
// Count every local candidate the page produces, per peer connection.
for (const p of [a, b]) {
  await p.page.evaluate(() => {
    window.__ice = { sent: 0, perPc: [] };
    const Orig = window.RTCPeerConnection;
    window.RTCPeerConnection = function (...args) {
      const pc = new Orig(...args);
      const slot = window.__ice.perPc.length;
      window.__ice.perPc.push(0);
      pc.addEventListener("icecandidate", () => {
        window.__ice.sent += 1;
        window.__ice.perPc[slot] += 1;
      });
      return pc;
    };
    window.RTCPeerConnection.prototype = Orig.prototype;
  });
}
await a.page.locator("#file-input").setInputFiles(join(dir, "piccolo.bin"));
await a.page.waitForTimeout(3000);
await b.page.locator(".tree-download, .offer-card button").first().click();
await b.page.waitForTimeout(12000);
for (const [name, p] of [["source", a], ["recipient", b]]) {
  const ice = await p.page.evaluate(() => window.__ice ?? null);
  console.log(`${engine} ${name}: total=${ice?.sent} perConnection=${JSON.stringify(ice?.perPc)}`);
}
await a.cleanup();
await b.cleanup();
env.cleanup();
rmSync(dir, { recursive: true, force: true });
process.exit(0);
