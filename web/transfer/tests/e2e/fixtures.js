// Shared browser instrumentation for the multipeer suites (2.6+).
//
// The app itself installs every recorder when `window.__BORE_TEST__` exists
// before app load (see src/main.js); this module only ensures the hook
// object exists and reads the counters back. Production runs without the
// hook: nothing is recorded, nothing changes.
export async function installTestHooks(context) {
  await context.addInitScript(() => {
    window.__BORE_TEST__ = window.__BORE_TEST__ ?? {};
  });
}

/** Snapshot of the app-side recorders (all zero/absent without the hook). */
export async function hookCounters(page) {
  return page.evaluate(() => {
    const hook = window.__BORE_TEST__ ?? {};
    return {
      outboundTypes: [...(hook.outboundTypes ?? [])],
      wsUrls: [...(hook.wsUrls ?? [])],
      fileReads: (hook.fileReads ?? []).length,
      rtc: hook.rtcConstructed ?? 0,
      transferRows: typeof hook.transferRows === "function" ? hook.transferRows() : 0,
    };
  });
}

/** Live catalog through the app hook (offerId/peerId/manifest/mac). */
export async function hookCatalog(page) {
  return page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot());
}
