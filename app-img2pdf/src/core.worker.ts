/// <reference lib="webworker" />

import init, { core_version, images_to_pdf } from "./wasm/localbench_core.js";

type WorkerRequest =
  | { id: number; type: "build"; buffers: ArrayBuffer[]; page: "fit" | "a4" | "letter" };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "built"; id: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;

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

  try {
    const buffers = request.buffers.map((buffer) => new Uint8Array(buffer));
    const result = images_to_pdf(buffers, request.page);
    const bytes = result.slice().buffer;
    scope.postMessage(
      { type: "built", id: request.id, bytes } satisfies WorkerResponse,
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
