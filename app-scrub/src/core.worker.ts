/// <reference lib="webworker" />

import {
  observeResourceTally,
  type ResourceProofRequest,
  type ResourceProofResponse,
} from "../../shared/resource-proof";
import init, {
  core_version,
  inspect_metadata,
  scrub_metadata,
  verify_metadata_removed,
} from "./wasm/localbench_core.js";

type WorkerRequest =
  | ResourceProofRequest
  | { id: number; type: "inspect"; bytes: ArrayBuffer }
  | { id: number; type: "scrub"; bytes: ArrayBuffer }
  | { id: number; type: "verify"; bytes: ArrayBuffer };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "inspected"; id: number; report: string }
  | { type: "result"; id: number; bytes: ArrayBuffer }
  | { type: "verified"; id: number }
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
  return typeof error === "string" ? error : "The file metadata could not be processed.";
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
    if (request.type === "inspect") {
      const report = inspect_metadata(new Uint8Array(request.bytes));
      scope.postMessage({ type: "inspected", id: request.id, report } satisfies WorkerResponse);
      return;
    }

    if (request.type === "verify") {
      /*
       * The INDEPENDENT check, not a second inspect_metadata. Re-inspecting
       * would ask the same detector the same question, so it could never see
       * anything that detector was already blind to — the reassurance could
       * not fail. verify_metadata_removed walks the file's structure instead
       * and throws with the specific container it found.
       */
      verify_metadata_removed(new Uint8Array(request.bytes));
      scope.postMessage({ type: "verified", id: request.id } satisfies WorkerResponse);
      return;
    }

    const result = scrub_metadata(new Uint8Array(request.bytes));
    const bytes = result.slice().buffer;
    scope.postMessage(
      { type: "result", id: request.id, bytes } satisfies WorkerResponse,
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
