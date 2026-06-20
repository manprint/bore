/**
 * Polling scheduler for the active panel.
 *
 * BUG-0: the old polling code dispatched a `panel:refresh` CustomEvent that
 * NOTHING listened to, so the UI never auto-refreshed. This module owns the
 * timer and calls `refreshFn` directly. It is decoupled from the global
 * `setInterval`/`clearInterval` (injectable) so it can be unit-tested headless.
 */

/** Default polling interval: 30 seconds. Data panels inherit this; config stays 0 (static). */
export const DEFAULT_REFRESH_MS = 30000;

function createDefaultTimers() {
    return {
        setInterval: (...args) => globalThis.setInterval(...args),
        clearInterval: (...args) => globalThis.clearInterval(...args),
    };
}

export function createPoller(refreshFn, timers = createDefaultTimers()) {
    let handle = null;
    return {
        /** (Re)arm a repeating call to `refreshFn` every `refreshMs` ms.
         *  `refreshMs <= 0` stops polling. Restarting clears the previous timer. */
        start(refreshMs) {
            if (handle != null) {
                timers.clearInterval(handle);
                handle = null;
            }
            if (refreshMs > 0) {
                handle = timers.setInterval(() => refreshFn(), refreshMs);
            }
        },
        stop() {
            if (handle != null) {
                timers.clearInterval(handle);
                handle = null;
            }
        },
        isRunning() {
            return handle != null;
        },
    };
}
