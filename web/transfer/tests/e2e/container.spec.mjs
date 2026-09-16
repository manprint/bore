// T-WEB-NOSTORE-CONTAINER / T-WEB-DEPLOY, browser half (6.5).
//
// The server here is NOT started by this process: it is the shipped container
// (or release binary) started by `scripts/web_transfer_container_test.sh`,
// with a READ-ONLY root filesystem and no volume of any kind. The room was
// created from the host with `bore transfer web`, and its URL arrives in
// `BORE_WEB_E2E_ROOM_URL`.
//
// What it proves that a host-side curl cannot: a real transfer completes
// through that server, end to end, with the bytes intact — so "read-only root
// filesystem" is not merely "the process starts", which is all a liveness
// probe would have shown.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { externalRoomEnv, openPersistentPeer, opfsWorks } from "./helpers.mjs";

const env = externalRoomEnv();

test.describe.serial("container", () => {
  test.skip(
    () => env === null,
    "no BORE_WEB_E2E_ROOM_URL: this spec drives a server someone else started",
  );

  test("T-WEB-NOSTORE-CONTAINER a transfer completes against the deployed server", async () => {
    test.setTimeout(180_000);
    const dir = mkdtempSync(join(tmpdir(), "bore-container-"));
    let a = null;
    let b = null;
    try {
      const bytes = Buffer.alloc(1024 * 1024 + 11);
      for (let i = 0; i < bytes.length; i += 1) {
        bytes[i] = (i * 17 + 3) % 251;
      }
      const hash = createHash("sha256").update(bytes).digest("hex");
      writeFileSync(join(dir, "deployed.bin"), bytes);

      const { browserName, defaultBrowserType, channel } = test.info().project.use;
      // Relay on purpose: the direct path is browser-to-browser and would
      // measure this machine's own WebRTC, not the deployment. The relay is
      // the leg that runs THROUGH the container.
      const open = () =>
        openPersistentPeer(env.roomUrl, {
          browserName: browserName ?? defaultBrowserType,
          channel,
          noWebRtc: true,
        });
      a = await open();
      b = await open();
      for (const peer of [a, b]) {
        await expect(peer.page.locator("#room-status")).toContainText("Connesso", {
          timeout: 30_000,
        });
      }
      expect(await opfsWorks(b.page)).toBe(true);

      await a.page.locator("#file-input").setInputFiles([join(dir, "deployed.bin")]);
      const started = Date.now();
      let offer = null;
      while (offer === null) {
        offer = await b.page.evaluate(() => {
          const catalog = window.__BORE_TEST__.getCatalogSnapshot();
          return catalog.length === 1 ? catalog[0] : null;
        });
        if (offer === null && Date.now() - started > 60_000) {
          throw new Error("the offer never reached the second peer");
        }
        if (offer === null) {
          await new Promise((r) => setTimeout(r, 200));
        }
      }
      await b.page.evaluate((id) => window.__BORE_TEST__.requestDownload(id), offer.offerId);
      await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 120_000 });

      const download = await Promise.all([
        b.page.waitForEvent("download", { timeout: 60_000 }),
        b.page.locator("#save-file").click(),
      ]).then(([event]) => event);
      const saved = readFileSync(await download.path());
      expect(saved.length).toBe(bytes.length);
      expect(createHash("sha256").update(saved).digest("hex")).toBe(hash);

      const commits = await b.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.at(-1).path).toBe("relay");
      expect(a.failures).toEqual([]);
      expect(b.failures).toEqual([]);
    } finally {
      for (const peer of [a, b]) {
        if (peer) {
          await peer.cleanup();
        }
      }
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
