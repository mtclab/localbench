import "./style.css";

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

type RequestResponse = Extract<WorkerResponse, { id: number }>;
type StatusState = "ready" | "working" | "success" | "error";

const MODEL_SPECS = [
  {
    key: "detection",
    url: "/models/text-detection.rten",
    expectedBytes: 2_510_284,
  },
  {
    key: "recognition",
    url: "/models/text-recognition.rten",
    expectedBytes: 9_716_568,
  },
] as const;
const MAX_IMAGE_BYTES = 64 * 1024 * 1024;
const SUPPORTED_MIME_TYPES = new Set([
  "image/jpeg",
  "image/png",
  "image/webp",
  "image/gif",
  "image/bmp",
]);
const SUPPORTED_EXTENSION = /\.(?:jpe?g|png|webp|gif|bmp)$/i;

function requiredElement<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) throw new Error(`Required interface element is missing: ${selector}`);
  return element;
}

const fileInput = requiredElement<HTMLInputElement>("#ocr-file-input");
const dropZone = requiredElement<HTMLDivElement>("#ocr-drop-zone");
const editor = requiredElement<HTMLElement>("#ocr-editor");
const sourceName = requiredElement<HTMLSpanElement>("#ocr-source-name");
const sourceSize = requiredElement<HTMLSpanElement>("#ocr-source-size");
const preview = requiredElement<HTMLImageElement>("#ocr-preview");
const previewDetail = requiredElement<HTMLElement>("#ocr-preview-detail");
const extractButton = requiredElement<HTMLButtonElement>("#extract-button");
const textModeInput = requiredElement<HTMLInputElement>("#output-mode-text");
const pdfModeInput = requiredElement<HTMLInputElement>("#output-mode-pdf");
const loadEngineButton = requiredElement<HTMLButtonElement>("#load-engine-button");
const status = requiredElement<HTMLDivElement>("#ocr-status");
const statusText = requiredElement<HTMLSpanElement>("#ocr-status-text");
const textOutput = requiredElement<HTMLElement>("#ocr-output");
const recognizedText = requiredElement<HTMLTextAreaElement>("#ocr-text");
const copyButton = requiredElement<HTMLButtonElement>("#copy-button");
const downloadButton = requiredElement<HTMLButtonElement>("#download-button");
const pdfOutput = requiredElement<HTMLElement>("#ocr-pdf-output");
const pdfDownloadButton = requiredElement<HTMLButtonElement>("#pdf-download-button");
const version = requiredElement<HTMLElement>("#core-version");
const engineStatus = requiredElement<HTMLElement>("#engine-status");

const worker = new Worker(new URL("./core.worker.ts", import.meta.url), { type: "module" });
const pending = new Map<
  number,
  { resolve: (response: RequestResponse) => void; reject: (reason: Error) => void }
>();
let nextRequestId = 1;
let workerAvailable = false;
let rejectWorkerReady: (reason: Error) => void = () => undefined;
let resolveWorkerReady: () => void = () => undefined;
const workerReady = new Promise<void>((resolve, reject) => {
  resolveWorkerReady = resolve;
  rejectWorkerReady = reject;
});
// Prevent an unhandled rejection if startup fails before the user interacts.
void workerReady.catch(() => undefined);

function rejectPending(reason: Error) {
  for (const request of pending.values()) request.reject(reason);
  pending.clear();
}

function failWorker(message = "The local OCR worker could not start.") {
  const error = new Error(message);
  workerAvailable = false;
  version.textContent = "Unavailable";
  engineStatus.textContent = "Unavailable";
  rejectWorkerReady(error);
  rejectPending(error);
  setStatus(message, "error");
  updateControls();
}

worker.addEventListener("message", (event: MessageEvent<WorkerResponse>) => {
  const response = event.data;
  if (response.type === "ready") {
    workerAvailable = true;
    version.textContent = `v${response.version}`;
    resolveWorkerReady();
    updateControls();
    return;
  }

  if (response.type === "error" && response.id === undefined) {
    failWorker(response.message);
    return;
  }

  if (response.id === undefined) {
    failWorker("The local OCR worker returned an invalid response.");
    return;
  }
  const responseId = response.id;
  const request = pending.get(responseId);
  if (!request) return;
  pending.delete(responseId);
  if (response.type === "error") {
    request.reject(new Error(response.message));
  } else {
    request.resolve(response);
  }
});

worker.addEventListener("error", () => failWorker());
worker.addEventListener("messageerror", () =>
  failWorker("The local OCR worker received an unreadable message."),
);

async function workerRequest(
  request: WorkerRequest,
  transfer: Transferable[],
): Promise<RequestResponse> {
  await workerReady;
  return new Promise<RequestResponse>((resolve, reject) => {
    const timer = window.setTimeout(() => {
      pending.delete(request.id);
      reject(new Error("Local OCR timed out. Try a smaller or clearer image."));
    }, 120_000);
    pending.set(request.id, {
      resolve: (response) => {
        window.clearTimeout(timer);
        resolve(response);
      },
      reject: (reason) => {
        window.clearTimeout(timer);
        reject(reason);
      },
    });
    worker.postMessage(request, transfer);
  });
}

async function loadModelsInWorker(
  detection: ArrayBuffer,
  recognition: ArrayBuffer,
): Promise<void> {
  const id = nextRequestId++;
  const response = await workerRequest(
    { id, type: "loadModels", detection, recognition },
    [detection, recognition],
  );
  if (response.type !== "modelsLoaded") {
    throw new Error("The OCR worker returned an unexpected model response.");
  }
}

async function recognizeInWorker(image: ArrayBuffer): Promise<string> {
  const id = nextRequestId++;
  const response = await workerRequest({ id, type: "ocr", image }, [image]);
  if (response.type !== "text") {
    throw new Error("The OCR worker returned an unexpected text response.");
  }
  return response.text;
}

async function createSearchablePdfInWorker(image: ArrayBuffer): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const response = await workerRequest({ id, type: "searchablePdf", image }, [image]);
  if (response.type !== "pdf") {
    throw new Error("The OCR worker returned an unexpected PDF response.");
  }
  return response.bytes;
}

function setStatus(text: string, state: StatusState) {
  statusText.textContent = text;
  status.dataset.state = state;
}

function formatFileSize(bytes: number): string {
  if (bytes < 1_000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1_000).toFixed(1)} KB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

type SelectedImage = { file: File; previewUrl: string };
let selectedImage: SelectedImage | null = null;
let engineLoaded = false;
let engineLoading = false;
let engineLoadPromise: Promise<void> | null = null;
let ocrWorking = false;
type OutputMode = "text" | "pdf";
let outputMode: OutputMode = "text";
let pdfDownloadUrl: string | null = null;
let pdfDownloadName = "searchable.pdf";

function updateControls() {
  const busy = engineLoading || ocrWorking;
  fileInput.disabled = busy || !workerAvailable;
  extractButton.disabled = busy || !workerAvailable || selectedImage === null;
  textModeInput.disabled = busy;
  pdfModeInput.disabled = busy;
  extractButton.textContent = ocrWorking
    ? outputMode === "text"
      ? "Extracting text…"
      : "Creating searchable PDF…"
    : outputMode === "text"
      ? "Extract text"
      : "Create searchable PDF";
  loadEngineButton.disabled = busy || !workerAvailable || engineLoaded;
  loadEngineButton.textContent = engineLoaded
    ? "OCR engine ready"
    : engineLoading
      ? "Loading OCR engine…"
      : "Load OCR engine";
  copyButton.disabled = ocrWorking || recognizedText.value.length === 0;
  downloadButton.disabled = ocrWorking || recognizedText.value.length === 0;
  pdfDownloadButton.disabled = ocrWorking || pdfDownloadUrl === null;
}

function revokePdfDownload() {
  if (!pdfDownloadUrl) return;
  URL.revokeObjectURL(pdfDownloadUrl);
  pdfDownloadUrl = null;
}

function clearOutput() {
  revokePdfDownload();
  recognizedText.value = "";
  textOutput.hidden = true;
  pdfOutput.hidden = true;
  copyButton.textContent = "Copy";
  updateControls();
}

for (const input of [textModeInput, pdfModeInput]) {
  input.addEventListener("change", () => {
    if (!input.checked) return;
    outputMode = input.value as OutputMode;
    clearOutput();
    if (selectedImage) {
      setStatus(
        `${selectedImage.file.name} is ready for ${
          outputMode === "text" ? "text extraction" : "a searchable PDF"
        }.`,
        "ready",
      );
    }
  });
}

function isSupportedImage(file: File): boolean {
  return (
    (file.type !== "" && SUPPORTED_MIME_TYPES.has(file.type.toLowerCase())) ||
    (file.type === "" && SUPPORTED_EXTENSION.test(file.name))
  );
}

function imageDimensions(image: HTMLImageElement): string {
  return `${image.naturalWidth} × ${image.naturalHeight} px`;
}

function selectImage(file: File) {
  if (engineLoading || ocrWorking) return;
  if (!workerAvailable) {
    setStatus("The local OCR core is unavailable. Reload the page and try again.", "error");
    fileInput.value = "";
    return;
  }
  if (!isSupportedImage(file)) {
    setStatus("Choose a JPEG, PNG, WebP, GIF, or BMP image.", "error");
    fileInput.value = "";
    return;
  }
  if (file.size === 0) {
    setStatus("That image is empty. Choose another file.", "error");
    fileInput.value = "";
    return;
  }
  if (file.size > MAX_IMAGE_BYTES) {
    setStatus("That encoded image is too large (maximum 64 MiB).", "error");
    fileInput.value = "";
    return;
  }

  if (selectedImage) URL.revokeObjectURL(selectedImage.previewUrl);
  const previewUrl = URL.createObjectURL(file);
  selectedImage = { file, previewUrl };
  clearOutput();
  editor.hidden = false;
  sourceName.textContent = file.name;
  sourceSize.textContent = formatFileSize(file.size);
  preview.hidden = true;
  preview.alt = `Preview of ${file.name}`;
  previewDetail.textContent = "Loading local preview…";
  preview.addEventListener(
    "load",
    () => {
      if (selectedImage?.previewUrl !== previewUrl) return;
      preview.hidden = false;
      previewDetail.textContent = `Local preview · ${imageDimensions(preview)}`;
      setStatus(
        `${file.name} is ready — ${imageDimensions(preview)}, ${formatFileSize(file.size)}.`,
        "ready",
      );
    },
    { once: true },
  );
  preview.addEventListener(
    "error",
    () => {
      if (selectedImage?.previewUrl !== previewUrl) return;
      preview.hidden = true;
      previewDetail.textContent = "Preview unavailable; the OCR core will validate the image.";
      setStatus(`${file.name} is ready for local validation.`, "ready");
    },
    { once: true },
  );
  preview.src = previewUrl;
  fileInput.value = "";
  updateControls();
}

fileInput.addEventListener("change", () => {
  const [file] = fileInput.files ?? [];
  if (file) selectImage(file);
});

for (const eventName of ["dragenter", "dragover"]) {
  dropZone.addEventListener(eventName, (event) => {
    event.preventDefault();
    dropZone.dataset.dragging = "true";
  });
}
for (const eventName of ["dragleave", "drop"]) {
  dropZone.addEventListener(eventName, (event) => {
    event.preventDefault();
    delete dropZone.dataset.dragging;
  });
}
dropZone.addEventListener("drop", (event) => {
  const [file] = Array.from(event.dataTransfer?.files ?? []);
  if (file) selectImage(file);
});

let serviceWorkerRegistration: Promise<ServiceWorkerRegistration> | null = null;

function registerServiceWorker(): Promise<ServiceWorkerRegistration> {
  if (!("serviceWorker" in navigator)) {
    return Promise.reject(new Error("Service workers are unavailable."));
  }
  serviceWorkerRegistration ??= navigator.serviceWorker.register("/sw.js");
  return serviceWorkerRegistration;
}

async function waitForServiceWorkerControl(): Promise<void> {
  if (!("serviceWorker" in navigator)) return;
  await registerServiceWorker();
  await navigator.serviceWorker.ready;
  if (navigator.serviceWorker.controller) return;

  await new Promise<void>((resolve) => {
    const timer = window.setTimeout(resolve, 5_000);
    navigator.serviceWorker.addEventListener(
      "controllerchange",
      () => {
        window.clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

type ModelProgress = { loaded: number; expected: number };

async function fetchModel(
  spec: (typeof MODEL_SPECS)[number],
  onProgress: (key: string, progress: ModelProgress) => void,
): Promise<ArrayBuffer> {
  const response = await fetch(spec.url, { credentials: "same-origin" });
  if (!response.ok) {
    throw new Error(`Could not download the OCR ${spec.key} model (HTTP ${response.status}).`);
  }

  if (!response.body) {
    const bytes = await response.arrayBuffer();
    onProgress(spec.key, { loaded: bytes.byteLength, expected: spec.expectedBytes });
    return bytes;
  }

  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let loaded = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.byteLength;
    onProgress(spec.key, { loaded, expected: spec.expectedBytes });
  }

  if (loaded === 0) throw new Error(`The OCR ${spec.key} model download was empty.`);
  const bytes = new Uint8Array(loaded);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes.buffer;
}

async function ensureEngine(): Promise<void> {
  if (engineLoaded) return;
  if (engineLoadPromise) return engineLoadPromise;

  engineLoadPromise = (async () => {
    engineLoading = true;
    engineStatus.textContent = "Downloading…";
    updateControls();
    setStatus("Downloading the offline OCR engine (~12 MB, one time)…", "working");

    // An active controller is what makes this first model fetch populate the
    // service worker's cache. OCR can still proceed if registration is blocked.
    await waitForServiceWorkerControl().catch(() => undefined);

    const progress = new Map<string, ModelProgress>();
    const onProgress = (key: string, modelProgress: ModelProgress) => {
      progress.set(key, modelProgress);
      let loaded = 0;
      let expected = 0;
      for (const value of progress.values()) loaded += value.loaded;
      for (const spec of MODEL_SPECS) expected += spec.expectedBytes;
      const percent = Math.min(100, Math.round((loaded / expected) * 100));
      setStatus(
        `Downloading the offline OCR engine (~12 MB, one time)… ${percent}%`,
        "working",
      );
    };

    const [detection, recognition] = await Promise.all(
      MODEL_SPECS.map((spec) => fetchModel(spec, onProgress)),
    );
    setStatus("Starting the local OCR engine…", "working");
    engineStatus.textContent = "Starting…";
    await loadModelsInWorker(detection, recognition);
    engineLoaded = true;
    engineStatus.textContent = "Ready offline";
  })();

  try {
    await engineLoadPromise;
  } finally {
    engineLoading = false;
    engineLoadPromise = null;
    if (!engineLoaded) engineStatus.textContent = "Not loaded";
    updateControls();
  }
}

loadEngineButton.addEventListener("click", async () => {
  if (engineLoaded || engineLoading || ocrWorking || !workerAvailable) return;
  try {
    await ensureEngine();
    setStatus("OCR engine ready. It is cached for offline use.", "success");
  } catch (error) {
    setStatus(
      error instanceof Error ? error.message : "The OCR engine could not be loaded.",
      "error",
    );
  }
});

extractButton.addEventListener("click", async () => {
  if (!selectedImage || engineLoading || ocrWorking || !workerAvailable) return;
  ocrWorking = true;
  clearOutput();
  updateControls();

  try {
    await ensureEngine();
    if (outputMode === "text") {
      setStatus(`Extracting text from ${selectedImage.file.name} locally…`, "working");
      const text = await recognizeInWorker(await selectedImage.file.arrayBuffer());
      recognizedText.value = text;
      textOutput.hidden = false;
      if (text.trim().length === 0) {
        setStatus(
          "OCR finished, but no text was recognized. Try a clearer, straighter image.",
          "ready",
        );
      } else {
        setStatus(
          "Text extracted locally. This is early-preview OCR — check the result.",
          "success",
        );
      }
    } else {
      setStatus(`Creating a searchable PDF from ${selectedImage.file.name} locally…`, "working");
      const bytes = await createSearchablePdfInWorker(
        await selectedImage.file.arrayBuffer(),
      );
      pdfDownloadUrl = URL.createObjectURL(new Blob([bytes], { type: "application/pdf" }));
      pdfDownloadName = searchablePdfFilename(selectedImage.file.name);
      pdfOutput.hidden = false;
      setStatus(
        "Searchable PDF created locally. This is early-preview OCR — check the result.",
        "success",
      );
    }
  } catch (error) {
    setStatus(
      error instanceof Error
        ? error.message
        : outputMode === "text"
          ? "Text could not be extracted from this image."
          : "A searchable PDF could not be created from this image.",
      "error",
    );
  } finally {
    ocrWorking = false;
    updateControls();
  }
});

function fallbackCopy(textarea: HTMLTextAreaElement): boolean {
  textarea.focus();
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.setSelectionRange(0, 0);
  return copied;
}

copyButton.addEventListener("click", async () => {
  if (recognizedText.value.length === 0) return;
  try {
    if (navigator.clipboard) {
      await navigator.clipboard.writeText(recognizedText.value);
    } else if (!fallbackCopy(recognizedText)) {
      throw new Error("Copy is unavailable in this browser.");
    }
    copyButton.textContent = "Copied";
    setStatus("Recognized text copied to the clipboard.", "success");
    window.setTimeout(() => {
      copyButton.textContent = "Copy";
    }, 1_500);
  } catch (error) {
    setStatus(error instanceof Error ? error.message : "The text could not be copied.", "error");
  }
});

function textFilename(filename: string): string {
  return `${filename.replace(/\.[^./]+$/, "") || "recognized-text"}.txt`;
}

function searchablePdfFilename(filename: string): string {
  return `${filename.replace(/\.[^./]+$/, "") || "image"}-searchable.pdf`;
}

downloadButton.addEventListener("click", () => {
  if (!selectedImage || recognizedText.value.length === 0) return;
  const url = URL.createObjectURL(
    new Blob([recognizedText.value], { type: "text/plain;charset=utf-8" }),
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = textFilename(selectedImage.file.name);
  link.hidden = true;
  document.body.append(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1_000);
});

pdfDownloadButton.addEventListener("click", () => {
  if (!pdfDownloadUrl) return;
  const link = document.createElement("a");
  link.href = pdfDownloadUrl;
  link.download = pdfDownloadName;
  link.hidden = true;
  document.body.append(link);
  link.click();
  link.remove();
});

const localBadge = requiredElement<HTMLButtonElement>("#local-badge");
const localInspector = requiredElement<HTMLDialogElement>("#local-inspector");
const inspectorClose = requiredElement<HTMLButtonElement>("#inspector-close");
const externalRequestCount = requiredElement<HTMLElement>("#external-request-count");
const cspConnectSrc = requiredElement<HTMLElement>("#csp-connect-src");
const offlineControlStatus = requiredElement<HTMLElement>("#offline-control-status");
const cspMeta = requiredElement<HTMLMetaElement>(
  'meta[http-equiv="Content-Security-Policy"]',
);
let inspectorReturnFocus: HTMLElement | null = null;

function updateInspectorProof() {
  const externalResources = performance.getEntriesByType("resource").filter((entry) => {
    try {
      return new URL(entry.name, location.href).origin !== location.origin;
    } catch {
      return true;
    }
  });
  externalRequestCount.textContent = String(externalResources.length);

  const connectDirective = cspMeta.content
    .split(";")
    .map((directive) => directive.trim())
    .find((directive) => /^connect-src(?:\s|$)/i.test(directive));
  cspConnectSrc.textContent = connectDirective ?? "Not declared";

  const controlled =
    "serviceWorker" in navigator && navigator.serviceWorker.controller !== null;
  offlineControlStatus.textContent = controlled
    ? "Yes — a service worker controls this page"
    : "Not yet — reload once after the first visit";
}

function focusableInspectorElements(): HTMLElement[] {
  return Array.from(
    localInspector.querySelectorAll<HTMLElement>(
      'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
    ),
  );
}

function restoreInspectorFocus() {
  const returnTarget = inspectorReturnFocus;
  inspectorReturnFocus = null;
  returnTarget?.focus();
}

function closeInspector() {
  localInspector.close();
  restoreInspectorFocus();
}

localBadge.addEventListener("click", () => {
  updateInspectorProof();
  inspectorReturnFocus = localBadge;
  localInspector.showModal();
  inspectorClose.focus();
});
inspectorClose.addEventListener("click", closeInspector);
localInspector.addEventListener("cancel", (event) => {
  event.preventDefault();
  closeInspector();
});
localInspector.addEventListener("click", (event) => {
  if (event.target === localInspector) closeInspector();
});
localInspector.addEventListener("keydown", (event) => {
  if (event.key !== "Tab") return;
  const focusable = focusableInspectorElements();
  if (focusable.length === 0) {
    event.preventDefault();
    return;
  }
  const first = focusable[0];
  const last = focusable[focusable.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
});
localInspector.addEventListener("close", restoreInspectorFocus);

if (location.hash !== "#ocr") {
  history.replaceState(null, "", `${location.pathname}${location.search}#ocr`);
}
document.title = "Image OCR — localbench";

const themeToggle = requiredElement<HTMLButtonElement>("#theme-toggle");
const themeLabel = requiredElement<HTMLSpanElement>("#theme-label");
const themeColor = requiredElement<HTMLMetaElement>('meta[name="theme-color"]');

function preferredTheme(): "light" | "dark" {
  const stored = localStorage.getItem("localbench-theme");
  if (stored === "light" || stored === "dark") return stored;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function applyTheme(theme: "light" | "dark") {
  document.documentElement.dataset.theme = theme;
  themeToggle.setAttribute("aria-pressed", String(theme === "dark"));
  themeToggle.setAttribute("aria-label", `Use ${theme === "dark" ? "light" : "dark"} theme`);
  themeLabel.textContent = theme === "dark" ? "Light" : "Dark";
  themeColor.content = theme === "dark" ? "#101722" : "#f5f7fb";
}

applyTheme(preferredTheme());
themeToggle.addEventListener("click", () => {
  const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  localStorage.setItem("localbench-theme", theme);
  applyTheme(theme);
});

window.addEventListener("beforeunload", () => {
  if (selectedImage) URL.revokeObjectURL(selectedImage.previewUrl);
  revokePdfDownload();
  worker.terminate();
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.addEventListener("controllerchange", () => {
    if (localInspector.open) updateInspectorProof();
  });
  window.addEventListener("load", () => {
    void registerServiceWorker().catch(() => undefined);
  });
}

setStatus("Choose an image to begin — it never leaves your device.", "ready");
updateControls();
