import "./style.css";

// Protocol mirrors app/src/core.worker.ts. The worker posts "ready" (with the
// Rust core version) once on load; every operation is id-matched.
type WorkerRequest =
  | { id: number; type: "page-count"; bytes: ArrayBuffer }
  | { id: number; type: "merge"; documents: ArrayBuffer[] };

type WorkerResult =
  | { type: "result"; id: number; pages: number }
  | { type: "result"; id: number; bytes: ArrayBuffer };

type WorkerResponse =
  | { type: "ready"; version: string }
  | WorkerResult
  | { type: "error"; id?: number; message: string };

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

  if (response.type === "result") {
    pending.get(response.id)?.resolve(response);
    pending.delete(response.id);
    return;
  }

  // error
  if (response.id === undefined) {
    onCoreFailed();
    return;
  }
  pending.get(response.id)?.reject(new Error(response.message));
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
  timeoutMs: number,
): Promise<WorkerResult> {
  return new Promise<WorkerResult>((resolve, reject) => {
    // Safety net: if the worker ever dies without firing "error" (e.g. a future
    // op panics and aborts the wasm instance), don't leave the UI hanging.
    const timer = setTimeout(() => {
      pending.delete(request.id);
      reject(new Error("Processing timed out."));
    }, timeoutMs);
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

async function countPages(bytes: ArrayBuffer): Promise<number> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "page-count", bytes }, [bytes], 30_000);
  if (!("pages" in result)) throw new Error("The local core returned an unexpected result.");
  return result.pages;
}

async function mergeDocuments(documents: ArrayBuffer[]): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest(
    { id, type: "merge", documents },
    documents,
    120_000,
  );
  if (!("bytes" in result)) throw new Error("The local core returned an unexpected result.");
  return result.bytes;
}

function requiredElement<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) throw new Error(`Required interface element is missing: ${selector}`);
  return element;
}

const countResult = requiredElement<HTMLDivElement>("#result");
const countResultText = requiredElement<HTMLSpanElement>("#result-text");
const countFileInput = requiredElement<HTMLInputElement>("#file-input");
const countDropZone = requiredElement<HTMLDivElement>("#drop-zone");
const mergeResult = requiredElement<HTMLDivElement>("#merge-result");
const mergeResultText = requiredElement<HTMLSpanElement>("#merge-result-text");
const mergeFileInput = requiredElement<HTMLInputElement>("#merge-file-input");
const mergeDropZone = requiredElement<HTMLDivElement>("#merge-drop-zone");
const mergeQueue = requiredElement<HTMLElement>("#merge-queue");
const mergeList = requiredElement<HTMLOListElement>("#merge-list");
const mergeSummary = requiredElement<HTMLSpanElement>("#merge-summary");
const mergeButton = requiredElement<HTMLButtonElement>("#merge-button");
const version = requiredElement<HTMLElement>("#core-version");

type StatusState = "ready" | "working" | "success" | "error";

function setStatus(
  result: HTMLDivElement,
  resultText: HTMLSpanElement,
  text: string,
  state: StatusState,
) {
  resultText.textContent = text;
  result.dataset.state = state;
}

function setCountStatus(text: string, state: StatusState) {
  setStatus(countResult, countResultText, text, state);
}

function setMergeStatus(text: string, state: StatusState) {
  setStatus(mergeResult, mergeResultText, text, state);
}

let coreFailed = false;

function onCoreReady(coreVersion: string) {
  coreFailed = false;
  version.textContent = `v${coreVersion}`;
  updateMergeControls();
}

function onCoreFailed() {
  coreFailed = true;
  version.textContent = "Unavailable";
  setCountStatus("The local processing core could not load.", "error");
  setMergeStatus("The local processing core could not load.", "error");
  updateMergeControls();
}

function looksLikePdf(file: File): boolean {
  return file.type === "application/pdf" || file.name.toLowerCase().endsWith(".pdf");
}

async function processCountFile(file: File) {
  if (!looksLikePdf(file)) {
    setCountStatus("Choose a PDF file to continue.", "error");
    return;
  }

  setCountStatus(`Reading ${file.name} locally…`, "working");
  try {
    // Reading and transferring are orchestration only; PDF byte interpretation stays in Rust.
    const bytes = await file.arrayBuffer();
    const count = await countPages(bytes);
    setCountStatus(`${count} ${count === 1 ? "page" : "pages"}`, "success");
  } catch (error) {
    const message = error instanceof Error ? error.message : "This PDF could not be read.";
    setCountStatus(message, "error");
  } finally {
    countFileInput.value = "";
  }
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

countFileInput.addEventListener("change", () => {
  const [file] = countFileInput.files ?? [];
  if (file) void processCountFile(file);
});

wireDropZone(countDropZone, ([file]) => {
  if (file) void processCountFile(file);
});

let selectedMergeFiles: File[] = [];
let mergeWorking = false;

function formatFileSize(bytes: number): string {
  if (bytes < 1_000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1_000).toFixed(1)} KB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

function updateMergeControls() {
  mergeButton.disabled = selectedMergeFiles.length === 0 || mergeWorking || coreFailed;
  mergeButton.textContent = mergeWorking ? "Merging…" : "Merge PDFs";
  mergeFileInput.disabled = mergeWorking || coreFailed;
}

function moveMergeFile(index: number, direction: -1 | 1) {
  const target = index + direction;
  if (target < 0 || target >= selectedMergeFiles.length || mergeWorking) return;
  [selectedMergeFiles[index], selectedMergeFiles[target]] = [
    selectedMergeFiles[target],
    selectedMergeFiles[index],
  ];
  renderMergeFiles();
  setMergeStatus("Order updated. Ready to merge locally.", "ready");
}

function removeMergeFile(index: number) {
  if (mergeWorking) return;
  selectedMergeFiles.splice(index, 1);
  renderMergeFiles();
  setMergeStatus(
    selectedMergeFiles.length === 0
      ? "Add PDFs to begin — they never leave your device."
      : `${selectedMergeFiles.length} ${selectedMergeFiles.length === 1 ? "PDF" : "PDFs"} ready to merge.`,
    "ready",
  );
}

function fileActionButton(label: string, symbol: string, onClick: () => void): HTMLButtonElement {
  const button = document.createElement("button");
  button.className = "file-action";
  button.type = "button";
  button.setAttribute("aria-label", label);
  button.title = label;
  button.textContent = symbol;
  button.addEventListener("click", onClick);
  return button;
}

function renderMergeFiles() {
  mergeList.replaceChildren();
  mergeQueue.hidden = selectedMergeFiles.length === 0;
  mergeSummary.textContent = `${selectedMergeFiles.length} ${selectedMergeFiles.length === 1 ? "file" : "files"}`;

  selectedMergeFiles.forEach((file, index) => {
    const item = document.createElement("li");
    item.className = "merge-file";

    const order = document.createElement("span");
    order.className = "file-order";
    order.textContent = String(index + 1);
    order.setAttribute("aria-hidden", "true");

    const details = document.createElement("span");
    details.className = "file-details";
    const name = document.createElement("strong");
    name.className = "file-name";
    name.textContent = file.name;
    const size = document.createElement("span");
    size.className = "file-size";
    size.textContent = formatFileSize(file.size);
    details.append(name, size);

    const actions = document.createElement("span");
    actions.className = "file-actions";
    const moveUp = fileActionButton(`Move ${file.name} up`, "↑", () => moveMergeFile(index, -1));
    moveUp.disabled = index === 0 || mergeWorking;
    const moveDown = fileActionButton(`Move ${file.name} down`, "↓", () => moveMergeFile(index, 1));
    moveDown.disabled = index === selectedMergeFiles.length - 1 || mergeWorking;
    const remove = fileActionButton(`Remove ${file.name}`, "×", () => removeMergeFile(index));
    remove.classList.add("remove-action");
    remove.disabled = mergeWorking;
    actions.append(moveUp, moveDown, remove);

    item.append(order, details, actions);
    mergeList.append(item);
  });

  updateMergeControls();
}

function addMergeFiles(files: File[]) {
  if (mergeWorking || coreFailed) return;

  const pdfs = files.filter(looksLikePdf);
  const rejectedCount = files.length - pdfs.length;
  selectedMergeFiles.push(...pdfs);
  renderMergeFiles();

  if (rejectedCount > 0) {
    setMergeStatus(
      `${rejectedCount} non-PDF ${rejectedCount === 1 ? "file was" : "files were"} skipped.`,
      "error",
    );
  } else if (pdfs.length > 0) {
    setMergeStatus(
      `${selectedMergeFiles.length} ${selectedMergeFiles.length === 1 ? "PDF" : "PDFs"} ready to merge.`,
      "ready",
    );
  }

  mergeFileInput.value = "";
}

mergeFileInput.addEventListener("change", () => {
  addMergeFiles(Array.from(mergeFileInput.files ?? []));
});
wireDropZone(mergeDropZone, addMergeFiles);

function downloadMergedPdf(bytes: ArrayBuffer) {
  const url = URL.createObjectURL(new Blob([bytes], { type: "application/pdf" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = "merged.pdf";
  link.hidden = true;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

mergeButton.addEventListener("click", async () => {
  if (selectedMergeFiles.length === 0 || mergeWorking || coreFailed) return;

  mergeWorking = true;
  renderMergeFiles();
  setMergeStatus(
    `Merging ${selectedMergeFiles.length} ${selectedMergeFiles.length === 1 ? "PDF" : "PDFs"} locally…`,
    "working",
  );

  try {
    // JS reads, transfers, and downloads bytes only; Rust owns all PDF interpretation.
    const documents = await Promise.all(selectedMergeFiles.map((file) => file.arrayBuffer()));
    const merged = await mergeDocuments(documents);
    downloadMergedPdf(merged);
    setMergeStatus("Merged PDF ready — downloaded as merged.pdf.", "success");
  } catch (error) {
    const message = error instanceof Error ? error.message : "The PDFs could not be merged.";
    setMergeStatus(message, "error");
  } finally {
    mergeWorking = false;
    renderMergeFiles();
  }
});

type Tool = "page-count" | "merge";
const toolButtons = Array.from(document.querySelectorAll<HTMLButtonElement>("[data-tool]"));
const toolPanels = Array.from(document.querySelectorAll<HTMLElement>("[data-tool-panel]"));

function switchTool(tool: Tool) {
  for (const button of toolButtons) {
    const active = button.dataset.tool === tool;
    if (active) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
  for (const panel of toolPanels) {
    panel.hidden = panel.dataset.toolPanel !== tool;
  }
}

for (const button of toolButtons) {
  button.addEventListener("click", () => {
    if (button.dataset.tool === "page-count" || button.dataset.tool === "merge") {
      switchTool(button.dataset.tool);
    }
  });
}

setCountStatus("Drop a PDF to count its pages — it never leaves your device.", "ready");
setMergeStatus("Add PDFs to begin — they never leave your device.", "ready");
renderMergeFiles();

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
