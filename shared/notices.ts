/*
 * Notices — the honesty layer, shared by every keeplocal app.
 *
 * The Rust core no longer changes a user's file in a way they would not expect
 * without saying so. Every operation that can transform bytes returns a
 * `FileResult`: the bytes, plus `notices` (sentences written for the user) and
 * `notice_codes` (stable identifiers in the same order).
 *
 * Two rules this module exists to enforce:
 *
 *  1. A notice is displayed VERBATIM. The core owns the wording; the interface
 *     owns only the frame around it. Nothing here rewrites, truncates, or
 *     summarises a message.
 *
 *  2. A notice cannot be dropped on the floor. `notices` is a REQUIRED field of
 *     the answer an app renders, so a result cannot reach the screen with its
 *     notices left behind — the type checker refuses to build it. The runtime
 *     half of that proof is scripts/notice-proof.mjs, which intercepts the real
 *     worker's messages in a real browser and fails if anything the core
 *     returned is missing from the visible page.
 *
 * Why some notices are louder than others: `pdf-returned-unchanged` and
 * `image-returned-unchanged` mean the advertised work did not happen to the
 * file the user just downloaded — they directly contradict a promise printed
 * elsewhere on the same page. Those escalate the whole result panel and are
 * repeated in the status strip under the button, because the failure mode we
 * are guarding against is a user who downloads and leaves.
 */

export type CoreNotice = {
  /** Stable identifier, e.g. "pdf-returned-unchanged". */
  code: string;
  /** The sentence to show the user, exactly as the core wrote it. */
  message: string;
};

/**
 * Codes whose message contradicts something the page promises. These are not
 * "extra detail" — they say the download is not what the interface claimed, so
 * they get the alarm treatment rather than a quiet footnote.
 */
export const CONTRADICTION_CODES: ReadonlySet<string> = new Set([
  "pdf-returned-unchanged",
  "image-returned-unchanged",
]);

/**
 * Pairs the core's parallel `notices` / `notice_codes` arrays. A message with
 * no matching code still travels (an unlabelled truth is still a truth); a code
 * with no message is dropped, since there would be nothing to show.
 */
export function toNotices(messages: readonly string[], codes: readonly string[]): CoreNotice[] {
  return messages.map((message, index) => ({ code: codes[index] ?? "", message }));
}

function contradictions(notices: readonly CoreNotice[]): CoreNotice[] {
  return notices.filter((notice) => CONTRADICTION_CODES.has(notice.code));
}

function hasContradiction(notices: readonly CoreNotice[]): boolean {
  return contradictions(notices).length > 0;
}

/**
 * What the status strip under the action button should say about a finished
 * operation. A contradiction replaces the app's cheerful success line with the
 * core's own sentence, verbatim: that strip is where the user is looking the
 * instant the download starts, and "Compressed PDF ready" would be a lie there.
 */
export function statusFromNotices(
  notices: readonly CoreNotice[],
  successText: string,
): { text: string; state: "success" | "notice" } {
  const alarming = contradictions(notices);
  if (alarming.length === 0) return { text: successText, state: "success" };
  return { text: alarming.map((notice) => notice.message).join(" "), state: "notice" };
}

/**
 * Renders notices into `container`, replacing whatever was there. An empty list
 * hides the container; it never leaves a stale notice attached to a new result.
 *
 * Returns true when at least one notice contradicts the page's copy, so the
 * caller can escalate the surrounding panel.
 */
export function renderNotices(container: HTMLElement, notices: readonly CoreNotice[]): boolean {
  container.replaceChildren();

  if (notices.length === 0) {
    container.hidden = true;
    delete container.dataset.level;
    return false;
  }

  const alarming = hasContradiction(notices);

  const title = document.createElement("p");
  title.className = "notices-title";
  title.textContent = alarming
    ? "Read this — your download is not what this page promised"
    : "What this did to your file";

  const list = document.createElement("ul");
  list.className = "notices-list";
  for (const notice of notices) {
    const item = document.createElement("li");
    item.className = "notice";
    if (notice.code) item.dataset.code = notice.code;
    if (CONTRADICTION_CODES.has(notice.code)) item.dataset.level = "contradiction";
    // textContent, never innerHTML: the message is the core's, shown as written.
    item.textContent = notice.message;
    list.append(item);
  }

  container.append(title, list);
  container.hidden = false;
  container.dataset.level = alarming ? "contradiction" : "info";
  return alarming;
}
