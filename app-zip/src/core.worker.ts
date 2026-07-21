/// <reference lib="webworker" />

import init, {
  core_version,
  create_zip,
  extract_zip_entry,
  list_zip,
} from "./wasm/localbench_core.js";

type WorkerRequest =
  | { id: number; type: "createZip"; names: string[]; buffers: ArrayBuffer[] }
  | { id: number; type: "listZip"; bytes: ArrayBuffer }
  | { id: number; type: "extractEntry"; bytes: ArrayBuffer; index: number };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "zipCreated"; id: number; bytes: ArrayBuffer }
  | { type: "zipListed"; id: number; report: string }
  | { type: "entryExtracted"; id: number; index: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : "The ZIP archive could not be processed.";
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
    if (request.type === "createZip") {
      const buffers = request.buffers.map((buffer) => new Uint8Array(buffer));
      const result = create_zip(request.names, buffers);
      const bytes = result.slice().buffer;
      scope.postMessage(
        { type: "zipCreated", id: request.id, bytes } satisfies WorkerResponse,
        [bytes],
      );
      return;
    }

    if (request.type === "listZip") {
      const report = list_zip(new Uint8Array(request.bytes));
      scope.postMessage({ type: "zipListed", id: request.id, report } satisfies WorkerResponse);
      return;
    }

    const result = extract_zip_entry(new Uint8Array(request.bytes), request.index);
    const bytes = result.slice().buffer;
    scope.postMessage(
      {
        type: "entryExtracted",
        id: request.id,
        index: request.index,
        bytes,
      } satisfies WorkerResponse,
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
