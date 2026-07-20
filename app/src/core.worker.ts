/// <reference lib="webworker" />

import init, {
  core_version,
  merge_pdfs,
  organize_pdf,
  pdf_page_count,
} from "./wasm/localbench_core.js";

type WorkerRequest =
  | { id: number; type: "page-count"; bytes: ArrayBuffer }
  | { id: number; type: "merge"; documents: ArrayBuffer[] }
  | {
      id: number;
      type: "organize";
      bytes: ArrayBuffer;
      pages: number[];
      rotations: number[];
    };

type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "result"; id: number; pages: number }
  | { type: "result"; id: number; bytes: ArrayBuffer }
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

  try {
    if (request.type === "page-count") {
      const pages = pdf_page_count(new Uint8Array(request.bytes));
      scope.postMessage({ type: "result", id: request.id, pages } satisfies WorkerResponse);
      return;
    }

    let result: Uint8Array;
    if (request.type === "merge") {
      result = merge_pdfs(
        request.documents.map((document) => new Uint8Array(document)),
      );
    } else {
      result = organize_pdf(
        new Uint8Array(request.bytes),
        new Uint32Array(request.pages),
        new Int32Array(request.rotations),
      );
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
