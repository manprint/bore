// T-WEB-SCAFFOLD: the committed dist serves the room shell on every engine.
// Static (no room link): the app boots to "Link incompleto" without opening
// any socket. Live room behavior lives in room.spec.mjs against a real
// server.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "@playwright/test";

const dist = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "dist");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
};

let server;
let baseUrl;

test.beforeAll(async () => {
  server = createServer(async (req, res) => {
    // Browsers always probe /favicon.ico; the shell ships none (no icon
    // decision at scaffold time), so answer 204 instead of failing the
    // clean-console assertion on harness noise.
    if (req.url === "/favicon.ico") {
      res.writeHead(204);
      res.end();
      return;
    }
    const path =
      req.url === "/"
        ? "/index.html"
        : req.url.startsWith("/transfer/assets/")
          ? `/${req.url.slice("/transfer/assets/".length)}`
          : req.url;
    const ext = path.slice(path.lastIndexOf("."));
    try {
      const body = await readFile(join(dist, path));
      res.writeHead(200, { "content-type": MIME[ext] ?? "application/octet-stream" });
      res.end(body);
    } catch {
      res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
      res.end("not found");
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  baseUrl = `http://127.0.0.1:${port}`;
});

test.afterAll(async () => {
  await new Promise((resolve) => server.close(resolve));
});

test("T-WEB-SCAFFOLD room shell without a link", async ({ page }) => {
  const failures = [];
  const sockets = [];
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error") {
      failures.push(`console: ${message.text()}`);
    }
  });
  page.on("websocket", (socket) => sockets.push(socket.url()));
  await page.goto(`${baseUrl}/`);
  await expect(page.locator("#room-status")).toContainText("Link incompleto");
  // No fragment, no storage, no socket: nothing leaves the page.
  expect(sockets).toEqual([]);
  assertClean(failures);
});

function assertClean(failures) {
  expect(failures, "no console/page errors on the static shell").toEqual([]);
}
