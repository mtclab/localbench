/// <reference lib="webworker" />

import { toNotices, type CoreNotice } from "../../shared/notices";
import {
  observeResourceTally,
  type ResourceProofRequest,
  type ResourceProofResponse,
} from "../../shared/resource-proof";
import init, { core_version, images_to_pdf } from "./wasm/localbench_core.js";

type WorkerRequest =
  | ResourceProofRequest
  | { id: number; type: "build"; buffers: ArrayBuffer[]; page: "fit" | "a4" | "letter" };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "built"; id: number; bytes: ArrayBuffer; notices: CoreNotice[] }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;

/*
 * The receipt reads this thread, not just the page. Registered BEFORE the WASM
 * init await below so the page still gets a truthful reading when the core
 * fails to load — a silent worker would otherwise be indistinguishable from a
 * worker that made zero requests.
 */
const readResourceTally = observeResourceTally();

scope.addEventListener("message", (event: MessageEvent<WorkerRequest>) => {
  if (event.data?.type !== "resource-proof") return;
  scope.postMessage({
    type: "resource-proof",
    id: event.data.id,
    tally: readResourceTally(),
  } satisfies ResourceProofResponse);
});

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : "The PDF could not be created.";
}

try {
  await init();
  scope.postMessage({ type: "ready", version: core_version() } satisfies WorkerResponse);
} catch (error) {
  scope.postMessage({ type: "error", message: errorMessage(error) } satisfies WorkerResponse);
}

scope.addEventListener("message", (event: MessageEvent<WorkerRequest>) => {
  const request = event.data;
  // Answered by the receipt listener above; never a core operation.
  if (request.type === "resource-proof") return;

  try {
    const buffers = request.buffers.map((buffer) => new Uint8Array(buffer));
    // images_to_pdf returns a FileResult: bytes PLUS the notices the interface
    // has to show. Both ride the same message, so there is no path on which a
    // result arrives without them.
    const result = images_to_pdf(buffers, request.page);
    const notices = toNotices(result.notices, result.notice_codes);
    const bytes = result.bytes.slice().buffer;
    // The FileResult owns WASM heap memory; the JS copies above are all the
    // page needs, so release it rather than leaking a buffer per operation.
    result.free();
    scope.postMessage(
      { type: "built", id: request.id, bytes, notices } satisfies WorkerResponse,
      [bytes],
    );
  } catch (error) {
    scope.postMessage({
      type: "error",
      id: request.id,
      message: errorMessage(error),
    } satisfies WorkerResponse);
  }
});
