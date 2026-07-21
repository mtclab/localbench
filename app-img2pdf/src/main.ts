import "./style.css";

type PageMode = "fit" | "a4" | "letter";
type WorkerRequest = {
  id: number;
  type: "build";
  buffers: ArrayBuffer[];
  page: PageMode;
};
type WorkerResult = { type: "built"; id: number; bytes: ArrayBuffer };
type WorkerResponse =
  | { type: "ready"; version: string }
  | WorkerResult
  | { type: "error"; id?: number; message: string };

type SelectedImage = {
  file: File;
  previewUrl: string;
};

const MAX_IMAGES = 500;
const MAX_TOTAL_BYTES = 512_000_000;
const ACCEPTED_IMAGE_TYPES = new Set([
  "image/jpeg",
  "image/png",
  "image/gif",
  "image/bmp",
  "image/webp",
]);
const ACCEPTED_IMAGE_EXTENSIONS = new Set(["jpg", "jpeg", "png", "gif", "bmp", "webp"]);

const worker = new Worker(new URL("./core.worker.ts", import.meta.url), { type: "module" });
const pending = new Map<
  number,
  { resolve: (result: WorkerResult) => void; reject: (reason: Error) => void }
>();
let nextRequestId = 1;

worker.addEventListener("message", (event: MessageEvent<WorkerResponse>) => {
  const response = event.data;

  if (response.type === "ready") {
    onCoreReady(response.version);
    return;
  }

  if (response.type === "error") {
    if (response.id === undefined) {
      onCoreFailed();
      return;
    }
    pending.get(response.id)?.reject(new Error(response.message));
    pending.delete(response.id);
    return;
  }

  pending.get(response.id)?.resolve(response);
  pending.delete(response.id);
});

worker.addEventListener("error", () => {
  onCoreFailed();
  for (const request of pending.values()) {
    request.reject(new Error("The local processing worker could not start."));
  }
  pending.clear();
});

function runCoreRequest(
  request: WorkerRequest,
  transfer: Transferable[],
): Promise<WorkerResult> {
  return new Promise<WorkerResult>((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(request.id);
      reject(new Error("Processing timed out."));
    }, 30_000);
    pending.set(request.id, {
      resolve: (result) => {
        clearTimeout(timer);
        resolve(result);
      },
      reject: (reason) => {
        clearTimeout(timer);
        reject(reason);
      },
    });
    worker.postMessage(request, transfer);
  });
}

async function buildPdf(buffers: ArrayBuffer[], page: PageMode): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "build", buffers, page }, buffers);
  if (result.type !== "built") {
    throw new Error("The local core returned an unexpected response.");
  }
  return result.bytes;
}

function requiredElement<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) throw new Error(`Required interface element is missing: ${selector}`);
  return element;
}

const createResult = requiredElement<HTMLDivElement>("#create-result");
const createResultText = requiredElement<HTMLSpanElement>("#create-result-text");
const createFileInput = requiredElement<HTMLInputElement>("#create-file-input");
const createDropZone = requiredElement<HTMLDivElement>("#create-drop-zone");
const createEditor = requiredElement<HTMLElement>("#create-editor");
const createTotal = requiredElement<HTMLSpanElement>("#create-total");
const createFileList = requiredElement<HTMLUListElement>("#create-file-list");
const pageSize = requiredElement<HTMLSelectElement>("#page-size");
const createButton = requiredElement<HTMLButtonElement>("#create-button");
const createOutput = requiredElement<HTMLDivElement>("#create-output");
const createOutputCount = requiredElement<HTMLElement>("#create-output-count");
const createOutputSize = requiredElement<HTMLElement>("#create-output-size");
const createDownloadButton = requiredElement<HTMLButtonElement>("#create-download-button");
const version = requiredElement<HTMLElement>("#core-version");

type StatusState = "ready" | "working" | "success" | "error";

function setStatus(text: string, state: StatusState) {
  createResultText.textContent = text;
  createResult.dataset.state = state;
}

let coreReady = false;
let coreFailed = false;
let createWorking = false;
let selectedImages: SelectedImage[] = [];
let createdPdf: ArrayBuffer | null = null;

function onCoreReady(coreVersion: string) {
  coreReady = true;
  coreFailed = false;
  version.textContent = `v${coreVersion}`;
  updateControls();
}

function onCoreFailed() {
  coreReady = false;
  coreFailed = true;
  version.textContent = "Unavailable";
  setStatus("The local processing core could not load.", "error");
  updateControls();
}

function wireDropZone(dropZone: HTMLDivElement, onFiles: (files: File[]) => void) {
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
    onFiles(Array.from(event.dataTransfer?.files ?? []));
  });
}

function formatFileSize(bytes: number): string {
  if (bytes < 1_000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1_000).toFixed(1)} KB`;
  if (bytes < 1_000_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`;
  return `${(bytes / 1_000_000_000).toFixed(2)} GB`;
}

function downloadBytes(bytes: ArrayBuffer, mime: string, filename: string) {
  const url = URL.createObjectURL(new Blob([bytes], { type: mime }));
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.hidden = true;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

function inputSize(): number {
  return selectedImages.reduce((total, image) => total + image.file.size, 0);
}

function isAcceptedImage(file: File): boolean {
  if (ACCEPTED_IMAGE_TYPES.has(file.type.toLowerCase())) return true;
  const extension = file.name.split(".").at(-1)?.toLowerCase() ?? "";
  return ACCEPTED_IMAGE_EXTENSIONS.has(extension);
}

function resetOutput() {
  createdPdf = null;
  createOutput.hidden = true;
  createOutputCount.textContent = "—";
  createOutputSize.textContent = "—";
}

function readyMessage(): string {
  if (selectedImages.length === 0) {
    return "Choose images to begin — they never leave your device.";
  }
  return `${selectedImages.length} ${selectedImages.length === 1 ? "image" : "images"} ready in PDF page order.`;
}

function updateControls() {
  const unavailable = createWorking || coreFailed || !coreReady;
  createFileInput.disabled = unavailable;
  pageSize.disabled = unavailable;
  createButton.disabled =
    unavailable || selectedImages.length === 0 || inputSize() > MAX_TOTAL_BYTES;
  createButton.textContent = createWorking ? "Creating…" : "Create PDF";
  createDownloadButton.disabled = createWorking || createdPdf === null;

  for (const button of createFileList.querySelectorAll<HTMLButtonElement>("button")) {
    const index = Number(button.dataset.index);
    const direction = button.dataset.direction;
    button.disabled =
      createWorking ||
      (direction === "up" && index === 0) ||
      (direction === "down" && index === selectedImages.length - 1);
  }
}

function moveImage(index: number, offset: -1 | 1) {
  const destination = index + offset;
  if (destination < 0 || destination >= selectedImages.length || createWorking) return;
  const [image] = selectedImages.splice(index, 1);
  selectedImages.splice(destination, 0, image);
  resetOutput();
  renderImages();
  setStatus(readyMessage(), "ready");
}

function removeImage(index: number) {
  if (createWorking) return;
  const [removed] = selectedImages.splice(index, 1);
  if (removed) URL.revokeObjectURL(removed.previewUrl);
  resetOutput();
  renderImages();
  setStatus(readyMessage(), "ready");
}

function imageAction(
  label: string,
  text: string,
  index: number,
  direction: "up" | "down" | "remove",
  onClick: () => void,
): HTMLButtonElement {
  const button = document.createElement("button");
  button.className = `file-action${direction === "remove" ? " remove-action" : ""}`;
  button.type = "button";
  button.textContent = text;
  button.dataset.index = String(index);
  button.dataset.direction = direction;
  button.setAttribute("aria-label", label);
  button.addEventListener("click", onClick);
  return button;
}

function renderImages() {
  createFileList.replaceChildren();
  createEditor.hidden = selectedImages.length === 0;
  createTotal.textContent =
    `${selectedImages.length} ${selectedImages.length === 1 ? "image" : "images"} · ${formatFileSize(inputSize())}`;

  selectedImages.forEach(({ file, previewUrl }, index) => {
    const item = document.createElement("li");
    item.className = "merge-file";

    const thumbnail = document.createElement("img");
    thumbnail.className = "file-order";
    thumbnail.src = previewUrl;
    thumbnail.alt = `Preview of ${file.name}`;

    const details = document.createElement("span");
    details.className = "file-details";
    const name = document.createElement("span");
    name.className = "file-name";
    name.textContent = file.name;
    const size = document.createElement("span");
    size.className = "file-size";
    size.textContent = `Page ${index + 1} · ${formatFileSize(file.size)}`;
    details.append(name, size);

    const actions = document.createElement("span");
    actions.className = "file-actions";
    actions.append(
      imageAction(`Move ${file.name} up`, "↑", index, "up", () => moveImage(index, -1)),
      imageAction(`Move ${file.name} down`, "↓", index, "down", () => moveImage(index, 1)),
      imageAction(`Remove ${file.name}`, "×", index, "remove", () => removeImage(index)),
    );

    item.append(thumbnail, details, actions);
    createFileList.append(item);
  });

  updateControls();
}

function addImages(files: File[]) {
  if (files.length === 0 || createWorking) return;
  const accepted = files.filter(isAcceptedImage);
  const rejected = files.length - accepted.length;
  selectedImages.push(
    ...accepted.map((file) => ({ file, previewUrl: URL.createObjectURL(file) })),
  );
  resetOutput();
  renderImages();

  if (selectedImages.length > MAX_IMAGES) {
    setStatus(`Choose no more than ${MAX_IMAGES} images at a time.`, "error");
  } else if (inputSize() > MAX_TOTAL_BYTES) {
    setStatus("Choose no more than 512 MB of images for one PDF.", "error");
  } else if (rejected > 0) {
    setStatus(
      `${rejected} unsupported ${rejected === 1 ? "file was" : "files were"} skipped. Use JPEG, PNG, GIF, BMP, or WebP.`,
      "error",
    );
  } else {
    setStatus(readyMessage(), "ready");
  }
}

createFileInput.addEventListener("change", () => {
  addImages(Array.from(createFileInput.files ?? []));
  createFileInput.value = "";
});
wireDropZone(createDropZone, addImages);

pageSize.addEventListener("change", () => {
  resetOutput();
  updateControls();
  setStatus(readyMessage(), "ready");
});

createButton.addEventListener("click", async () => {
  if (selectedImages.length === 0 || createWorking || !coreReady || coreFailed) return;
  if (inputSize() > MAX_TOTAL_BYTES) {
    setStatus("Choose no more than 512 MB of images for one PDF.", "error");
    return;
  }

  createWorking = true;
  resetOutput();
  updateControls();
  setStatus(
    `Creating a ${selectedImages.length}-page PDF entirely on this device…`,
    "working",
  );

  try {
    const buffers = await Promise.all(
      selectedImages.map(({ file }) => file.arrayBuffer()),
    );
    const pdf = await buildPdf(buffers, pageSize.value as PageMode);
    createdPdf = pdf;
    createOutputCount.textContent = String(selectedImages.length);
    createOutputSize.textContent = formatFileSize(pdf.byteLength);
    createOutput.hidden = false;
    setStatus(
      `combined.pdf is ready — ${formatFileSize(pdf.byteLength)} with ${selectedImages.length} ${selectedImages.length === 1 ? "page" : "pages"}.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The PDF could not be created.";
    setStatus(message, "error");
  } finally {
    createWorking = false;
    updateControls();
  }
});

createDownloadButton.addEventListener("click", () => {
  if (createdPdf) downloadBytes(createdPdf, "application/pdf", "combined.pdf");
});

window.addEventListener("beforeunload", () => {
  for (const image of selectedImages) URL.revokeObjectURL(image.previewUrl);
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

localBadge.addEventListener("click", () => {
  updateInspectorProof();
  inspectorReturnFocus = localBadge;
  localInspector.showModal();
  inspectorClose.focus();
});

function restoreInspectorFocus() {
  const returnTarget = inspectorReturnFocus;
  inspectorReturnFocus = null;
  returnTarget?.focus();
}

function closeInspector() {
  localInspector.close();
  restoreInspectorFocus();
}

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

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.addEventListener("controllerchange", () => {
    if (localInspector.open) updateInspectorProof();
  });
}

setStatus("Choose images to begin — they never leave your device.", "ready");
renderImages();

const themeToggle = document.querySelector<HTMLButtonElement>("#theme-toggle");
const themeLabel = document.querySelector<HTMLSpanElement>("#theme-label");
const themeColor = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');

function preferredTheme(): "light" | "dark" {
  const stored = localStorage.getItem("localbench-theme");
  if (stored === "light" || stored === "dark") return stored;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function applyTheme(theme: "light" | "dark") {
  document.documentElement.dataset.theme = theme;
  themeToggle?.setAttribute("aria-pressed", String(theme === "dark"));
  themeToggle?.setAttribute("aria-label", `Use ${theme === "dark" ? "light" : "dark"} theme`);
  if (themeLabel) themeLabel.textContent = theme === "dark" ? "Light" : "Dark";
  if (themeColor) themeColor.content = theme === "dark" ? "#101722" : "#f5f7fb";
}

applyTheme(preferredTheme());
themeToggle?.addEventListener("click", () => {
  const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  localStorage.setItem("localbench-theme", theme);
  applyTheme(theme);
});

if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    void navigator.serviceWorker.register("/sw.js");
  });
}
