/// <reference lib="webworker" />

import { toNotices, type CoreNotice } from "../../shared/notices";
import {
  observeResourceTally,
  type ResourceProofRequest,
  type ResourceProofResponse,
} from "../../shared/resource-proof";
import init, {
  compress_image,
  convert_image,
  core_version,
  FileResult,
  resize_image,
} from "./wasm/localbench_core.js";

type WorkerRequest =
  | ResourceProofRequest
  | { id: number; type: "resize"; bytes: ArrayBuffer; maxW: number; maxH: number; keepAspect: boolean }
  | { id: number; type: "convert"; bytes: ArrayBuffer; target: "png" | "jpeg" | "webp" }
  | { id: number; type: "compress"; bytes: ArrayBuffer; quality: number };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "result"; id: number; bytes: ArrayBuffer; notices: CoreNotice[] }
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
  return typeof error === "string" ? error : "The image could not be processed.";
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
    // Every one of these returns a FileResult: bytes PLUS the notices the
    // interface has to show. The notices ride the same message as the bytes so
    // there is no path on which a result arrives without them.
    let result: FileResult;
    if (request.type === "resize") {
      result = resize_image(
        new Uint8Array(request.bytes),
        request.maxW,
        request.maxH,
        request.keepAspect,
      );
    } else if (request.type === "convert") {
      result = convert_image(new Uint8Array(request.bytes), request.target);
    } else {
      result = compress_image(new Uint8Array(request.bytes), request.quality);
    }
    const notices = toNotices(result.notices, result.notice_codes);
    const bytes = result.bytes.slice().buffer;
    // The FileResult owns WASM heap memory; the JS copies above are all the
    // page needs, so release it rather than leaking a buffer per operation.
    result.free();
    scope.postMessage(
      { type: "result", id: request.id, bytes, notices } satisfies WorkerResponse,
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
