import "./style.css";

// Protocol mirrors app/src/core.worker.ts. The worker posts "ready" (with the
// Rust core version) once on load; every operation is id-matched.
type WorkerRequest =
  | { id: number; type: "page-count"; bytes: ArrayBuffer }
  | { id: number; type: "merge"; documents: ArrayBuffer[] }
  | { id: number; type: "compress"; bytes: ArrayBuffer; quality: number }
  | {
      id: number;
      type: "organize";
      bytes: ArrayBuffer;
      pages: number[];
      rotations: number[];
    };

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

async function organizeDocument(
  bytes: ArrayBuffer,
  pages: number[],
  rotations: number[],
): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest(
    { id, type: "organize", bytes, pages, rotations },
    [bytes],
    120_000,
  );
  if (!("bytes" in result)) throw new Error("The local core returned an unexpected result.");
  return result.bytes;
}

async function compressDocument(bytes: ArrayBuffer, quality: number): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest(
    { id, type: "compress", bytes, quality },
    [bytes],
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
const organizeResult = requiredElement<HTMLDivElement>("#organize-result");
const organizeResultText = requiredElement<HTMLSpanElement>("#organize-result-text");
const organizeFileInput = requiredElement<HTMLInputElement>("#organize-file-input");
const organizeDropZone = requiredElement<HTMLDivElement>("#organize-drop-zone");
const organizeEditor = requiredElement<HTMLElement>("#organize-editor");
const organizeSourceName = requiredElement<HTMLSpanElement>("#organize-source-name");
const organizeSummary = requiredElement<HTMLSpanElement>("#organize-summary");
const pageAddForm = requiredElement<HTMLFormElement>("#page-add-form");
const pageRangeInput = requiredElement<HTMLInputElement>("#page-range-input");
const addPagesButton = requiredElement<HTMLButtonElement>("#add-pages-button");
const resetPagesButton = requiredElement<HTMLButtonElement>("#reset-pages-button");
const organizeList = requiredElement<HTMLOListElement>("#organize-list");
const organizeButton = requiredElement<HTMLButtonElement>("#organize-button");
const compressResult = requiredElement<HTMLDivElement>("#compress-result");
const compressResultText = requiredElement<HTMLSpanElement>("#compress-result-text");
const compressFileInput = requiredElement<HTMLInputElement>("#compress-file-input");
const compressDropZone = requiredElement<HTMLDivElement>("#compress-drop-zone");
const compressEditor = requiredElement<HTMLElement>("#compress-editor");
const compressSourceName = requiredElement<HTMLSpanElement>("#compress-source-name");
const compressSourceSize = requiredElement<HTMLSpanElement>("#compress-source-size");
const qualityPresets = requiredElement<HTMLFieldSetElement>("#quality-presets");
const compressButton = requiredElement<HTMLButtonElement>("#compress-button");
const compressOutput = requiredElement<HTMLDivElement>("#compress-output");
const compressBeforeSize = requiredElement<HTMLElement>("#compress-before-size");
const compressAfterSize = requiredElement<HTMLElement>("#compress-after-size");
const compressSavedPercent = requiredElement<HTMLElement>("#compress-saved-percent");
const compressDownloadButton = requiredElement<HTMLButtonElement>("#compress-download-button");
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

function setOrganizeStatus(text: string, state: StatusState) {
  setStatus(organizeResult, organizeResultText, text, state);
}

function setCompressStatus(text: string, state: StatusState) {
  setStatus(compressResult, compressResultText, text, state);
}

let coreFailed = false;

function onCoreReady(coreVersion: string) {
  coreFailed = false;
  version.textContent = `v${coreVersion}`;
  updateMergeControls();
  updateOrganizeControls();
  updateCompressControls();
}

function onCoreFailed() {
  coreFailed = true;
  version.textContent = "Unavailable";
  setCountStatus("The local processing core could not load.", "error");
  setMergeStatus("The local processing core could not load.", "error");
  setOrganizeStatus("The local processing core could not load.", "error");
  setCompressStatus("The local processing core could not load.", "error");
  updateMergeControls();
  updateOrganizeControls();
  updateCompressControls();
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

function downloadPdf(bytes: ArrayBuffer, filename: string) {
  const url = URL.createObjectURL(new Blob([bytes], { type: "application/pdf" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
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
    downloadPdf(merged, "merged.pdf");
    setMergeStatus("Merged PDF ready — downloaded as merged.pdf.", "success");
  } catch (error) {
    const message = error instanceof Error ? error.message : "The PDFs could not be merged.";
    setMergeStatus(message, "error");
  } finally {
    mergeWorking = false;
    renderMergeFiles();
  }
});

type OrganizeEntry = { page: number; rotation: 0 | 90 | 180 | 270 };

let organizeSource: { file: File; pageCount: number } | null = null;
let organizeEntries: OrganizeEntry[] = [];
let organizeLoading = false;
let organizeWorking = false;

function updateOrganizeControls() {
  const unavailable = organizeLoading || organizeWorking || coreFailed;
  organizeFileInput.disabled = unavailable;
  pageRangeInput.disabled = unavailable || organizeSource === null;
  addPagesButton.disabled = unavailable || organizeSource === null;
  resetPagesButton.disabled = unavailable || organizeSource === null;
  organizeButton.disabled = unavailable || organizeSource === null || organizeEntries.length === 0;
  organizeButton.textContent = organizeWorking ? "Exporting…" : "Export organized PDF";
}

function setOrganizeReadyStatus(message = "Output updated. Ready to export locally.") {
  if (!organizeWorking) setOrganizeStatus(message, "ready");
}

function moveOrganizePage(index: number, direction: -1 | 1) {
  const target = index + direction;
  if (target < 0 || target >= organizeEntries.length || organizeWorking) return;
  [organizeEntries[index], organizeEntries[target]] = [
    organizeEntries[target],
    organizeEntries[index],
  ];
  renderOrganizePages();
  setOrganizeReadyStatus("Page order updated. Ready to export locally.");
}

function removeOrganizePage(index: number) {
  if (organizeWorking) return;
  organizeEntries.splice(index, 1);
  renderOrganizePages();
  setOrganizeReadyStatus(
    organizeEntries.length === 0
      ? "The output is empty. Add pages before exporting."
      : "Page removed from the output. Ready to export locally.",
  );
}

function renderOrganizePages() {
  organizeList.replaceChildren();
  organizeEditor.hidden = organizeSource === null;
  organizeSourceName.textContent = organizeSource?.file.name ?? "";
  organizeSummary.textContent = organizeSource
    ? `${organizeEntries.length} output ${organizeEntries.length === 1 ? "page" : "pages"} · ${organizeSource.pageCount} source ${organizeSource.pageCount === 1 ? "page" : "pages"}`
    : "";

  if (organizeSource && organizeEntries.length === 0) {
    const empty = document.createElement("li");
    empty.className = "page-list-empty";
    empty.textContent = "No output pages yet. Add a page number or range above.";
    organizeList.append(empty);
  }

  organizeEntries.forEach((entry, index) => {
    const item = document.createElement("li");
    item.className = "organize-page";

    const order = document.createElement("span");
    order.className = "file-order";
    order.textContent = String(index + 1);
    order.setAttribute("aria-label", `Output position ${index + 1}`);

    const details = document.createElement("span");
    details.className = "file-details";
    const pageName = document.createElement("strong");
    pageName.className = "file-name";
    pageName.textContent = `Page ${entry.page}`;
    const sourceDetail = document.createElement("span");
    sourceDetail.className = "file-size";
    sourceDetail.textContent = `Source page ${entry.page}`;
    details.append(pageName, sourceDetail);

    const rotationLabel = document.createElement("label");
    rotationLabel.className = "rotation-control";
    const rotationText = document.createElement("span");
    rotationText.textContent = "Rotate";
    const rotationSelect = document.createElement("select");
    rotationSelect.setAttribute("aria-label", `Rotation for output page ${index + 1}, source page ${entry.page}`);
    for (const rotation of [0, 90, 180, 270] as const) {
      const option = document.createElement("option");
      option.value = String(rotation);
      option.textContent = `${rotation}°`;
      option.selected = entry.rotation === rotation;
      rotationSelect.append(option);
    }
    rotationSelect.disabled = organizeWorking;
    rotationSelect.addEventListener("change", () => {
      entry.rotation = Number(rotationSelect.value) as OrganizeEntry["rotation"];
      setOrganizeReadyStatus(`Page ${entry.page} rotation updated. Ready to export locally.`);
    });
    rotationLabel.append(rotationText, rotationSelect);

    const actions = document.createElement("span");
    actions.className = "file-actions";
    const moveUp = fileActionButton(`Move source page ${entry.page} up`, "↑", () =>
      moveOrganizePage(index, -1),
    );
    moveUp.disabled = index === 0 || organizeWorking;
    const moveDown = fileActionButton(`Move source page ${entry.page} down`, "↓", () =>
      moveOrganizePage(index, 1),
    );
    moveDown.disabled = index === organizeEntries.length - 1 || organizeWorking;
    const remove = fileActionButton(`Remove source page ${entry.page}`, "×", () =>
      removeOrganizePage(index),
    );
    remove.classList.add("remove-action");
    remove.disabled = organizeWorking;
    actions.append(moveUp, moveDown, remove);

    item.append(order, details, rotationLabel, actions);
    organizeList.append(item);
  });

  updateOrganizeControls();
}

function parsePageSelection(value: string, pageCount: number): number[] {
  const parts = value.split(",").map((part) => part.trim());
  if (parts.length === 0 || parts.some((part) => part.length === 0)) {
    throw new Error("Enter a page number or range, such as 1-3,5.");
  }

  const selected: number[] = [];
  for (const part of parts) {
    const match = /^(\d+)(?:\s*-\s*(\d+))?$/.exec(part);
    if (!match) throw new Error(`“${part}” is not a page number or range.`);
    const start = Number(match[1]);
    const end = Number(match[2] ?? match[1]);
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)) {
      throw new Error("A page number is too large.");
    }
    if (start < 1 || start > pageCount || end < 1 || end > pageCount) {
      throw new Error(`Choose pages between 1 and ${pageCount}.`);
    }
    const direction = start <= end ? 1 : -1;
    for (let page = start; ; page += direction) {
      selected.push(page);
      if (page === end) break;
    }
  }
  return selected;
}

async function processOrganizeFile(file: File) {
  if (!looksLikePdf(file)) {
    setOrganizeStatus("Choose a PDF file to continue.", "error");
    organizeFileInput.value = "";
    return;
  }
  if (organizeLoading || organizeWorking || coreFailed) return;

  organizeLoading = true;
  organizeSource = null;
  organizeEntries = [];
  renderOrganizePages();
  setOrganizeStatus(`Reading ${file.name} locally…`, "working");

  try {
    const pages = await countPages(await file.arrayBuffer());
    if (pages === 0) throw new Error("This PDF has no pages to organize.");
    organizeSource = { file, pageCount: pages };
    organizeEntries = Array.from(
      { length: pages },
      (_, index): OrganizeEntry => ({ page: index + 1, rotation: 0 }),
    );
    renderOrganizePages();
    setOrganizeStatus(
      `${file.name} has ${pages} ${pages === 1 ? "page" : "pages"}. All pages are in the output list.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "This PDF could not be read.";
    setOrganizeStatus(message, "error");
  } finally {
    organizeLoading = false;
    organizeFileInput.value = "";
    updateOrganizeControls();
  }
}

organizeFileInput.addEventListener("change", () => {
  const [file] = organizeFileInput.files ?? [];
  if (file) void processOrganizeFile(file);
});
wireDropZone(organizeDropZone, ([file]) => {
  if (file) void processOrganizeFile(file);
});

pageAddForm.addEventListener("submit", (event) => {
  event.preventDefault();
  if (!organizeSource || organizeWorking) return;
  try {
    const pages = parsePageSelection(pageRangeInput.value, organizeSource.pageCount);
    organizeEntries.push(...pages.map((page): OrganizeEntry => ({ page, rotation: 0 })));
    pageRangeInput.value = "";
    renderOrganizePages();
    setOrganizeReadyStatus(
      `${pages.length} ${pages.length === 1 ? "page" : "pages"} added to the output.`,
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "Those pages could not be added.";
    setOrganizeStatus(message, "error");
  }
});

resetPagesButton.addEventListener("click", () => {
  if (!organizeSource || organizeWorking) return;
  organizeEntries = Array.from(
    { length: organizeSource.pageCount },
    (_, index): OrganizeEntry => ({ page: index + 1, rotation: 0 }),
  );
  renderOrganizePages();
  setOrganizeReadyStatus("All source pages restored in their original order.");
});

organizeButton.addEventListener("click", async () => {
  if (!organizeSource || organizeEntries.length === 0 || organizeWorking || coreFailed) return;

  organizeWorking = true;
  renderOrganizePages();
  setOrganizeStatus(
    `Exporting ${organizeEntries.length} ${organizeEntries.length === 1 ? "page" : "pages"} locally…`,
    "working",
  );

  try {
    const organized = await organizeDocument(
      await organizeSource.file.arrayBuffer(),
      organizeEntries.map((entry) => entry.page),
      organizeEntries.map((entry) => entry.rotation),
    );
    downloadPdf(organized, "organized.pdf");
    setOrganizeStatus("Organized PDF ready — downloaded as organized.pdf.", "success");
  } catch (error) {
    const message = error instanceof Error ? error.message : "The PDF could not be organized.";
    setOrganizeStatus(message, "error");
  } finally {
    organizeWorking = false;
    renderOrganizePages();
  }
});

let compressSource: { file: File; pageCount: number } | null = null;
let compressedResult: { bytes: ArrayBuffer; filename: string } | null = null;
let compressLoading = false;
let compressWorking = false;

function updateCompressControls() {
  const unavailable = compressLoading || compressWorking || coreFailed;
  compressFileInput.disabled = unavailable;
  qualityPresets.disabled = unavailable || compressSource === null;
  compressButton.disabled = unavailable || compressSource === null;
  compressButton.textContent = compressWorking ? "Compressing…" : "Compress PDF";
  compressDownloadButton.disabled = compressWorking || compressedResult === null;
}

function renderCompressSource() {
  compressEditor.hidden = compressSource === null;
  compressSourceName.textContent = compressSource?.file.name ?? "";
  compressSourceSize.textContent = compressSource ? formatFileSize(compressSource.file.size) : "";
  updateCompressControls();
}

function resetCompressedResult() {
  compressedResult = null;
  compressOutput.hidden = true;
  compressBeforeSize.textContent = "—";
  compressAfterSize.textContent = "—";
  compressSavedPercent.textContent = "—";
  updateCompressControls();
}

async function processCompressFile(file: File) {
  if (!looksLikePdf(file)) {
    setCompressStatus("Choose a PDF file to continue.", "error");
    compressFileInput.value = "";
    return;
  }
  if (compressLoading || compressWorking || coreFailed) return;

  compressLoading = true;
  compressSource = null;
  resetCompressedResult();
  renderCompressSource();
  setCompressStatus(`Reading ${file.name} locally…`, "working");

  try {
    const pages = await countPages(await file.arrayBuffer());
    if (pages === 0) throw new Error("This PDF has no pages to compress.");
    compressSource = { file, pageCount: pages };
    renderCompressSource();
    setCompressStatus(
      `${file.name} is ready — ${pages} ${pages === 1 ? "page" : "pages"}, ${formatFileSize(file.size)}.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "This PDF could not be read.";
    setCompressStatus(message, "error");
  } finally {
    compressLoading = false;
    compressFileInput.value = "";
    renderCompressSource();
  }
}

compressFileInput.addEventListener("change", () => {
  const [file] = compressFileInput.files ?? [];
  if (file) void processCompressFile(file);
});
wireDropZone(compressDropZone, ([file]) => {
  if (file) void processCompressFile(file);
});

qualityPresets.addEventListener("change", () => {
  if (!compressSource || compressWorking) return;
  resetCompressedResult();
  setCompressStatus("Quality updated. Ready to compress locally.", "ready");
});

compressButton.addEventListener("click", async () => {
  if (!compressSource || compressWorking || coreFailed) return;
  const qualityInput = qualityPresets.querySelector<HTMLInputElement>(
    'input[name="compress-quality"]:checked',
  );
  const quality = Number(qualityInput?.value);
  if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
    setCompressStatus("Choose a compression quality preset.", "error");
    return;
  }

  compressWorking = true;
  resetCompressedResult();
  updateCompressControls();
  setCompressStatus(`Compressing ${compressSource.file.name} locally…`, "working");

  try {
    const beforeBytes = compressSource.file.size;
    const compressed = await compressDocument(
      await compressSource.file.arrayBuffer(),
      quality,
    );
    const afterBytes = compressed.byteLength;
    const savedBytes = Math.max(0, beforeBytes - afterBytes);
    const savedPercent = beforeBytes === 0 ? 0 : (savedBytes / beforeBytes) * 100;
    const baseName = compressSource.file.name.replace(/\.pdf$/i, "") || "document";
    const filename = `${baseName}-compressed.pdf`;

    compressedResult = { bytes: compressed, filename };
    compressBeforeSize.textContent = formatFileSize(beforeBytes);
    compressAfterSize.textContent = formatFileSize(afterBytes);
    compressSavedPercent.textContent = `${savedPercent.toFixed(1)}% (${formatFileSize(savedBytes)})`;
    compressOutput.hidden = false;
    downloadPdf(compressed, filename);
    setCompressStatus(
      savedBytes > 0
        ? `Compressed PDF ready — saved ${savedPercent.toFixed(1)}% and downloaded as ${filename}.`
        : `No safe size reduction was found. An unchanged-size PDF was downloaded as ${filename}.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The PDF could not be compressed.";
    setCompressStatus(message, "error");
  } finally {
    compressWorking = false;
    updateCompressControls();
  }
});

compressDownloadButton.addEventListener("click", () => {
  if (compressedResult) downloadPdf(compressedResult.bytes, compressedResult.filename);
});

type Tool = "page-count" | "merge" | "organize" | "compress";
const TOOL_TITLES: Record<Tool, string> = {
  "page-count": "Count PDF Pages — localbench",
  merge: "Merge PDFs — localbench",
  organize: "Organize PDF Pages — localbench",
  compress: "Compress PDF — localbench",
};
const toolButtons = Array.from(document.querySelectorAll<HTMLButtonElement>("[data-tool]"));
const toolPanels = Array.from(document.querySelectorAll<HTMLElement>("[data-tool-panel]"));
const toolSwitcher = requiredElement<HTMLElement>(".tool-switcher");
const openGraphTitle = requiredElement<HTMLMetaElement>('meta[property="og:title"]');
const twitterTitle = requiredElement<HTMLMetaElement>('meta[name="twitter:title"]');

function isTool(value: string | undefined): value is Tool {
  return (
    value === "page-count" ||
    value === "merge" ||
    value === "organize" ||
    value === "compress"
  );
}

function toolFromHash(): Tool | null {
  const value = location.hash.slice(1);
  return isTool(value) ? value : null;
}

function switchTool(tool: Tool, updateHash = false) {
  for (const button of toolButtons) {
    const active = button.dataset.tool === tool;
    button.tabIndex = active ? 0 : -1;
    if (active) {
      button.setAttribute("aria-current", "page");
    } else {
      button.removeAttribute("aria-current");
    }
  }
  for (const panel of toolPanels) {
    panel.hidden = panel.dataset.toolPanel !== tool;
  }

  const title = TOOL_TITLES[tool];
  document.title = title;
  openGraphTitle.content = title;
  twitterTitle.content = title;

  if (updateHash && location.hash !== `#${tool}`) {
    history.pushState(null, "", `${location.pathname}${location.search}#${tool}`);
  }
}

for (const button of toolButtons) {
  button.addEventListener("click", () => {
    if (isTool(button.dataset.tool)) switchTool(button.dataset.tool, true);
  });
}

toolSwitcher.addEventListener("keydown", (event) => {
  const currentIndex = toolButtons.findIndex((button) => button === document.activeElement);
  if (currentIndex < 0) return;

  let nextIndex: number | null = null;
  if (event.key === "ArrowRight" || event.key === "ArrowDown") {
    nextIndex = (currentIndex + 1) % toolButtons.length;
  } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
    nextIndex = (currentIndex - 1 + toolButtons.length) % toolButtons.length;
  } else if (event.key === "Home") {
    nextIndex = 0;
  } else if (event.key === "End") {
    nextIndex = toolButtons.length - 1;
  }

  if (nextIndex === null) return;
  event.preventDefault();
  const nextButton = toolButtons[nextIndex];
  if (isTool(nextButton.dataset.tool)) switchTool(nextButton.dataset.tool, true);
  nextButton.focus();
});

function restoreToolFromHash() {
  const tool = toolFromHash() ?? "page-count";
  switchTool(tool);
  if (location.hash !== `#${tool}`) {
    history.replaceState(null, "", `${location.pathname}${location.search}#${tool}`);
  }
}

window.addEventListener("hashchange", restoreToolFromHash);
window.addEventListener("popstate", restoreToolFromHash);
restoreToolFromHash();

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

setCountStatus("Drop a PDF to count its pages — it never leaves your device.", "ready");
setMergeStatus("Add PDFs to begin — they never leave your device.", "ready");
setOrganizeStatus("Choose a PDF to begin — it never leaves your device.", "ready");
setCompressStatus("Choose a PDF to begin — it never leaves your device.", "ready");
renderMergeFiles();
renderOrganizePages();
renderCompressSource();

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
