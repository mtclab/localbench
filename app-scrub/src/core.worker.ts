/// <reference lib="webworker" />

import init, {
  core_version,
  inspect_metadata,
  scrub_metadata,
} from "./wasm/localbench_core.js";

type WorkerRequest =
  | { id: number; type: "inspect"; bytes: ArrayBuffer }
  | { id: number; type: "scrub"; bytes: ArrayBuffer };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "inspected"; id: number; report: string }
  | { type: "result"; id: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;

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

  try {
    if (request.type === "inspect") {
      const report = inspect_metadata(new Uint8Array(request.bytes));
      scope.postMessage({ type: "inspected", id: request.id, report } satisfies WorkerResponse);
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
