/// <reference lib="webworker" />

import init, { core_version, pdf_page_count } from "./wasm/localbench_core.js";

type WorkerRequest = {
  id: number;
  type: "page-count";
  bytes: ArrayBuffer;
};

type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "result"; id: number; pages: number }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return typeof error === "string" ? error : "The PDF could not be read.";
}

try {
  await init();
  scope.postMessage({ type: "ready", version: core_version() } satisfies WorkerResponse);
} catch (error) {
  scope.postMessage({ type: "error", message: errorMessage(error) } satisfies WorkerResponse);
}

scope.addEventListener("message", (event: MessageEvent<WorkerRequest>) => {
  const request = event.data;
  if (request.type !== "page-count") return;

  try {
    const pages = pdf_page_count(new Uint8Array(request.bytes));
    scope.postMessage({ type: "result", id: request.id, pages } satisfies WorkerResponse);
  } catch (error) {
    scope.postMessage({
      type: "error",
      id: request.id,
      message: errorMessage(error),
    } satisfies WorkerResponse);
  }
});

