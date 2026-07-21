/// <reference lib="webworker" />

import init, {
  core_version,
  engine_ready,
  load_engine,
  run_ocr,
  searchable_pdf,
} from "./wasm/localbench_ocr_core.js";

type WorkerRequest =
  | { id: number; type: "loadModels"; detection: ArrayBuffer; recognition: ArrayBuffer }
  | { id: number; type: "ocr"; image: ArrayBuffer }
  | { id: number; type: "searchablePdf"; image: ArrayBuffer };

type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "modelsLoaded"; id: number }
  | { type: "text"; id: number; text: string }
  | { type: "pdf"; id: number; bytes: ArrayBuffer }
  | { type: "error"; id?: number; message: string };

const scope: DedicatedWorkerGlobalScope = self as unknown as DedicatedWorkerGlobalScope;
let wasmReady = false;

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "The local OCR worker could not complete the request.";
}

try {
  await init();
  wasmReady = true;
  scope.postMessage({ type: "ready", version: core_version() } satisfies WorkerResponse);
} catch (error) {
  scope.postMessage({ type: "error", message: errorMessage(error) } satisfies WorkerResponse);
}

scope.addEventListener("message", (event: MessageEvent<WorkerRequest>) => {
  const request = event.data;

  try {
    if (!wasmReady) throw new Error("The local OCR core is unavailable.");

    if (request.type === "loadModels") {
      load_engine(
        new Uint8Array(request.detection),
        new Uint8Array(request.recognition),
      );
      if (!engine_ready()) throw new Error("The OCR engine did not become ready.");
      scope.postMessage({ type: "modelsLoaded", id: request.id } satisfies WorkerResponse);
      return;
    }

    if (!engine_ready()) throw new Error("The OCR engine is not loaded.");
    if (request.type === "ocr") {
      const text = run_ocr(new Uint8Array(request.image));
      scope.postMessage({ type: "text", id: request.id, text } satisfies WorkerResponse);
      return;
    }

    const pdf = searchable_pdf(new Uint8Array(request.image));
    const bytes = pdf.slice().buffer;
    scope.postMessage(
      { type: "pdf", id: request.id, bytes } satisfies WorkerResponse,
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
