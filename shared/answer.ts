/*
 * The answer block, shared by every keeplocal app.
 *
 * This lived as five byte-identical copies. It is one file now for the same
 * reason shared/identity.css is one file — but here the stake is higher than
 * consistency: this is the single place where a finished result is put on
 * screen, so it is the single place that can guarantee a result never appears
 * without the notices the core attached to it.
 *
 * `Answer.notices` is REQUIRED, not optional. A call site that has bytes but no
 * notices does not type-check, which is the compile-time half of the gate; the
 * runtime half is scripts/notice-proof.mjs.
 */

import { renderNotices, type CoreNotice } from "./notices";

export type Answer = {
  /** The produced filename, when there is one. */
  file?: string;
  /** The reading: the number or size that IS the result. */
  value: string;
  /** One line of context under the reading. */
  note?: string;
  /**
   * Everything the core said about what it did to this file. Required: an
   * answer without its notices is the defect this module exists to prevent.
   */
  notices: CoreNotice[];
};

/**
 * The answer is the hero. Once a tool has produced a result, the filename and
 * the number take the top of the panel and the sales headline steps down (the
 * demotion itself is CSS, keyed off data-answered).
 *
 * Passing `null` clears the block, its notices, and any escalation with it, so
 * a new operation can never inherit the previous one's warning — or worse, be
 * read under the previous one's clean bill of health.
 */
export function setAnswer(block: HTMLElement, answer: Answer | null): void {
  const panel = block.closest<HTMLElement>("[data-tool-panel]");
  const noticeSlot = block.querySelector<HTMLElement>("[data-answer-notices]");
  if (!noticeSlot) {
    // Loud on purpose. A missing slot means notices have nowhere to go, and
    // silently rendering the answer anyway is exactly the failure this guards.
    throw new Error("Required interface element is missing: [data-answer-notices]");
  }

  if (answer === null) {
    renderNotices(noticeSlot, []);
    delete block.dataset.notice;
    block.hidden = true;
    delete panel?.dataset.answered;
    return;
  }

  const fileSlot = block.querySelector<HTMLElement>("[data-answer-file]");
  const valueSlot = block.querySelector<HTMLElement>("[data-answer-value]");
  const noteSlot = block.querySelector<HTMLElement>("[data-answer-note]");

  if (fileSlot) {
    fileSlot.textContent = answer.file ?? "";
    fileSlot.hidden = answer.file === undefined;
  }
  if (valueSlot) valueSlot.textContent = answer.value;
  if (noteSlot) {
    noteSlot.textContent = answer.note ?? "";
    noteSlot.hidden = answer.note === undefined;
  }

  const contradicted = renderNotices(noticeSlot, answer.notices);
  if (contradicted) block.dataset.notice = "contradiction";
  else delete block.dataset.notice;

  block.hidden = false;
  if (panel) panel.dataset.answered = "true";

  // The failure mode being guarded against is a user who downloads and leaves.
  // The download has already started by now, so bring the statement about it
  // into view rather than leaving it above the fold the user never returns to.
  if (answer.notices.length > 0) {
    block.scrollIntoView({ behavior: "smooth", block: "nearest" });
  }
}
