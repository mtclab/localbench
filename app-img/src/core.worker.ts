/// <reference lib="webworker" />

import init, {
  compress_image,
  convert_image,
  core_version,
  resize_image,
} from "./wasm/localbench_core.js";

type WorkerRequest =
  | { id: number; type: "resize"; bytes: ArrayBuffer; maxW: number; maxH: number; keepAspect: boolean }
  | { id: number; type: "convert"; bytes: ArrayBuffer; target: "png" | "jpeg" | "webp" }
  | { id: number; type: "compress"; bytes: ArrayBuffer; quality: number };
type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "result"; id: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;

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

  try {
    let result: Uint8Array;
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
