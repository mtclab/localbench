/*
 * The receipt's request counter, shared by the window and the core worker.
 *
 * Why this module exists: a dedicated worker has its OWN Resource Timing
 * timeline. `performance.getEntriesByType("resource")` called in the window
 * cannot see a single request made by the worker — and the worker is the thread
 * that holds every byte of the user's file. A window-only count is therefore
 * blind to the one context that matters, so both globals tally themselves and
 * the window adds them up.
 *
 * Why a PerformanceObserver rather than getEntriesByType(): the resource timing
 * buffer silently caps (250 entries by default) and stops recording once full.
 * A capped buffer under-reports, and an under-counting receipt is worse than no
 * receipt. Entries delivered to an observer are not subject to that cap.
 *
 * What this still cannot see (stated on the page, not hidden here):
 *  - WebSocket and WebRTC connections are never reported as resource entries.
 *  - A request blocked by CSP produces no entry at all, so "blocked" and "never
 *    attempted" are indistinguishable from this side.
 * Those two gaps are why the policy rows sit next to the count on the receipt:
 * the count is corroboration, the policy is the binding constraint.
 */

export type ResourceTally = {
  /**
   * False when this global cannot observe Resource Timing at all. The receipt
   * must then say the number is unproven — never fall back to a reassuring 0.
   */
  measurable: boolean;
  /** Every resource entry this global has seen, same-origin included. */
  total: number;
  /** URLs of entries whose origin is not this global's origin. */
  external: string[];
};

export type ResourceTallyReader = () => ResourceTally;

export type ResourceProofRequest = { id: number; type: "resource-proof" };

export type ResourceProofResponse = {
  id: number;
  type: "resource-proof";
  tally: ResourceTally;
};

/**
 * Starts observing this global's resource timeline and returns a reader that
 * flushes any undelivered records before answering, so the number is current as
 * of the moment it is read.
 */
export function observeResourceTally(): ResourceTallyReader {
  const origin = self.location.origin;
  const external = new Set<string>();
  let total = 0;

  function record(entries: PerformanceEntryList): void {
    for (const entry of entries) {
      total += 1;
      try {
        // An entry name that will not parse cannot be shown to be same-origin,
        // so it counts as external. Unproven never resolves in our favour.
        if (new URL(entry.name, origin).origin !== origin) external.add(entry.name);
      } catch {
        external.add(entry.name);
      }
    }
  }

  let observer: PerformanceObserver | null = null;
  try {
    observer = new PerformanceObserver((list) => record(list.getEntries()));
    // buffered: true replays entries that happened before this line ran.
    observer.observe({ type: "resource", buffered: true });
  } catch {
    observer = null;
  }

  return () => {
    if (!observer) return { measurable: false, total: 0, external: [] };
    // takeRecords() drains records queued but not yet handed to the callback;
    // they are removed from the queue, so nothing is counted twice.
    record(observer.takeRecords());
    return { measurable: true, total, external: [...external] };
  };
}
