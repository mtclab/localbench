import "./style.css";
import { setAnswer } from "../../shared/answer";
import {
  observeResourceTally,
  type ResourceProofRequest,
  type ResourceProofResponse,
  type ResourceTally,
} from "../../shared/resource-proof";

// Started before the worker so the receipt's counter covers this document from
// its first request. See shared/resource-proof.ts for why an observer, not
// performance.getEntriesByType().
const readPageResourceTally = observeResourceTally();

type WorkerRequest =
  | ResourceProofRequest
  | { id: number; type: "createZip"; names: string[]; buffers: ArrayBuffer[] }
  | { id: number; type: "listZip"; bytes: ArrayBuffer }
  | { id: number; type: "extractEntry"; bytes: ArrayBuffer; index: number };

type WorkerResult =
  | { type: "zipCreated"; id: number; bytes: ArrayBuffer }
  | { type: "zipListed"; id: number; report: string }
  | { type: "entryExtracted"; id: number; index: number; bytes: ArrayBuffer };

type WorkerResponse =
  | { type: "ready"; version: string }
  | ResourceProofResponse
  | WorkerResult
  | { type: "error"; id?: number; message: string };

/*
 * One row of the core's archive listing. `extractable` is the field that
 * matters most: the core decides, while reading the central directory, whether
 * this entry can actually be handed over — encryption, an unsupported
 * compression method, or a size past the per-entry cap all make it false. The
 * interface must not offer a Download button that is guaranteed to fail.
 */
type ArchiveEntry = {
  name: string;
  size: number;
  compressed: number;
  is_dir: boolean;
  unsafe_path: boolean;
  encrypted: boolean;
  duplicate_name: boolean;
  extractable: boolean;
};

/** The whole listing: entries, the total uncompressed size, and the archive-wide warnings. */
type ArchiveListing = {
  entries: ArchiveEntry[];
  totalSize: number;
  warnings: string[];
};

const MAX_ARCHIVE_TOTAL_BYTES = 512_000_000;
const worker = new Worker(new URL("./core.worker.ts", import.meta.url), { type: "module" });
const pending = new Map<
  number,
  { resolve: (result: WorkerResult) => void; reject: (reason: Error) => void }
>();
// Kept apart from `pending` so a receipt probe can never be mistaken for a
// file operation, and vice versa.
const pendingResourceProof = new Map<number, (tally: ResourceTally | null) => void>();
let workerUnavailable = false;
let nextRequestId = 1;

worker.addEventListener("message", (event: MessageEvent<WorkerResponse>) => {
  const response = event.data;

  if (response.type === "ready") {
    onCoreReady(response.version);
    return;
  }

  if (response.type === "resource-proof") {
    pendingResourceProof.get(response.id)?.(response.tally);
    pendingResourceProof.delete(response.id);
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
  workerUnavailable = true;
  for (const request of pending.values()) {
    request.reject(new Error("The local processing worker could not start."));
  }
  pending.clear();
  // null, never 0: a worker that cannot answer has not been shown to be quiet.
  for (const resolve of pendingResourceProof.values()) resolve(null);
  pendingResourceProof.clear();
});

/**
 * Asks the worker for its own resource tally. Resolves null — not an empty
 * tally — when the worker does not answer, so the receipt can say "unproven"
 * instead of printing a zero it cannot stand behind.
 */
function requestWorkerResourceTally(timeoutMs = 1500): Promise<ResourceTally | null> {
  if (workerUnavailable) return Promise.resolve(null);
  return new Promise((resolve) => {
    const id = nextRequestId++;
    const timer = setTimeout(() => {
      pendingResourceProof.delete(id);
      resolve(null);
    }, timeoutMs);
    pendingResourceProof.set(id, (tally) => {
      clearTimeout(timer);
      resolve(tally);
    });
    worker.postMessage({ id, type: "resource-proof" } satisfies WorkerRequest);
  });
}

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

async function createArchive(names: string[], buffers: ArrayBuffer[]): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "createZip", names, buffers }, buffers);
  if (result.type !== "zipCreated") {
    throw new Error("The local core returned an unexpected response.");
  }
  return result.bytes;
}

async function listArchive(bytes: ArrayBuffer): Promise<string> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "listZip", bytes }, [bytes]);
  if (result.type !== "zipListed") {
    throw new Error("The local core returned an unexpected response.");
  }
  return result.report;
}

async function extractArchiveEntry(bytes: ArrayBuffer, index: number): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "extractEntry", bytes, index }, [bytes]);
  if (result.type !== "entryExtracted" || result.index !== index) {
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
const createButton = requiredElement<HTMLButtonElement>("#create-button");
const createOutput = requiredElement<HTMLDivElement>("#create-output");
const createOutputCount = requiredElement<HTMLElement>("#create-output-count");
const createOutputSize = requiredElement<HTMLElement>("#create-output-size");
const createDownloadButton = requiredElement<HTMLButtonElement>("#create-download-button");

const extractResult = requiredElement<HTMLDivElement>("#extract-result");
const extractResultText = requiredElement<HTMLSpanElement>("#extract-result-text");
const extractFileInput = requiredElement<HTMLInputElement>("#extract-file-input");
const extractDropZone = requiredElement<HTMLDivElement>("#extract-drop-zone");
const extractEditor = requiredElement<HTMLElement>("#extract-editor");
const extractSourceName = requiredElement<HTMLSpanElement>("#extract-source-name");
const extractSourceSize = requiredElement<HTMLSpanElement>("#extract-source-size");
const extractEntryCount = requiredElement<HTMLSpanElement>("#extract-entry-count");
const extractEntryTable = requiredElement<HTMLDivElement>("#extract-entry-table");
const extractWarnings = requiredElement<HTMLDivElement>("#extract-warnings");
const extractWarningList = requiredElement<HTMLUListElement>("#extract-warning-list");
const extractAllButton = requiredElement<HTMLButtonElement>("#extract-all-button");
const version = requiredElement<HTMLElement>("#core-version");

// "notice": the operation worked, but something about the archive is true
// that the page's own copy would otherwise gloss over. See shared/notices.ts.
type StatusState = "ready" | "working" | "success" | "error" | "notice";

function setStatus(
  result: HTMLDivElement,
  resultText: HTMLSpanElement,
  text: string,
  state: StatusState,
) {
  resultText.textContent = text;
  result.dataset.state = state;
}

function setCreateStatus(text: string, state: StatusState) {
  setStatus(createResult, createResultText, text, state);
}

function setExtractStatus(text: string, state: StatusState) {
  setStatus(extractResult, extractResultText, text, state);
}

/* The answer block lives in shared/answer.ts; see the note there on why. */

/* A drop zone that holds a file says which file, not "Drop a file here". */
function setDropLabel(zone: HTMLElement, title: string, detail: string, loaded: boolean) {
  const titleSlot = zone.querySelector<HTMLElement>(".drop-title");
  const detailSlot = zone.querySelector<HTMLElement>(".drop-detail");
  if (titleSlot) titleSlot.textContent = title;
  if (detailSlot) detailSlot.textContent = detail;
  if (loaded) zone.dataset.loaded = "true";
  else delete zone.dataset.loaded;
}

const createAnswer = requiredElement<HTMLElement>("#create-answer");
const extractAnswer = requiredElement<HTMLElement>("#extract-answer");

let coreReady = false;
let coreFailed = false;

function onCoreReady(coreVersion: string) {
  coreReady = true;
  coreFailed = false;
  version.textContent = `v${coreVersion}`;
  delete version.dataset.state;
  updateCreateControls();
  updateExtractControls();
}

function onCoreFailed() {
  coreReady = false;
  coreFailed = true;
  version.textContent = "Unavailable";
  setCreateStatus("The local processing core could not load.", "error");
  setExtractStatus("The local processing core could not load.", "error");
  updateCreateControls();
  updateExtractControls();
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

let createFiles: File[] = [];
let createdArchive: ArrayBuffer | null = null;
let createWorking = false;

function createInputSize(): number {
  return createFiles.reduce((total, file) => total + file.size, 0);
}

function resetCreateOutput() {
  createdArchive = null;
  setAnswer(createAnswer, null);
  createOutput.hidden = true;
  createOutputCount.textContent = "—";
  createOutputSize.textContent = "—";
}

function updateCreateControls() {
  const total = createInputSize();
  const unavailable = createWorking || coreFailed || !coreReady;
  createFileInput.disabled = unavailable;
  createButton.disabled = unavailable || createFiles.length === 0 || total > MAX_ARCHIVE_TOTAL_BYTES;
  createButton.textContent = createWorking ? "Creating…" : "Create .zip";
  createDownloadButton.disabled = createWorking || createdArchive === null;

  for (const button of createFileList.querySelectorAll<HTMLButtonElement>("button")) {
    button.disabled = createWorking;
  }
}

function renderCreateFiles() {
  createFileList.replaceChildren();
  createEditor.hidden = createFiles.length === 0;
  createTotal.textContent = `${createFiles.length} ${createFiles.length === 1 ? "file" : "files"} · ${formatFileSize(createInputSize())}`;

  if (createFiles.length === 0) {
    setDropLabel(createDropZone, "Drop files here", "or choose files from your device", false);
  } else {
    setDropLabel(
      createDropZone,
      createFiles.length === 1
        ? createFiles[0].name
        : `${createFiles.length} files ready to archive`,
      "Drop more files to add them",
      true,
    );
  }

  createFiles.forEach((file, index) => {
    const item = document.createElement("li");
    item.className = "merge-file";

    const order = document.createElement("span");
    order.className = "file-order";
    order.textContent = String(index + 1);

    const details = document.createElement("span");
    details.className = "file-details";
    const name = document.createElement("span");
    name.className = "file-name";
    name.textContent = file.name;
    const size = document.createElement("span");
    size.className = "file-size";
    size.textContent = formatFileSize(file.size);
    details.append(name, size);

    const actions = document.createElement("span");
    actions.className = "file-actions";
    const remove = document.createElement("button");
    remove.className = "file-action remove-action";
    remove.type = "button";
    remove.textContent = "×";
    remove.setAttribute("aria-label", `Remove ${file.name}`);
    remove.addEventListener("click", () => {
      createFiles.splice(index, 1);
      resetCreateOutput();
      renderCreateFiles();
      setCreateStatus(
        createFiles.length === 0
          ? "Choose files to begin — they never leave your device."
          : `${createFiles.length} ${createFiles.length === 1 ? "file" : "files"} ready to archive locally.`,
        "ready",
      );
    });
    actions.append(remove);
    item.append(order, details, actions);
    createFileList.append(item);
  });

  updateCreateControls();
}

function addCreateFiles(files: File[]) {
  if (files.length === 0 || createWorking) return;
  createFiles.push(...files);
  resetCreateOutput();
  renderCreateFiles();
  const total = createInputSize();
  if (total > MAX_ARCHIVE_TOTAL_BYTES) {
    setCreateStatus("Choose no more than 512 MB of files for one archive.", "error");
  } else {
    setCreateStatus(
      `${createFiles.length} ${createFiles.length === 1 ? "file" : "files"} ready to archive locally.`,
      "ready",
    );
  }
}

createFileInput.addEventListener("change", () => {
  addCreateFiles(Array.from(createFileInput.files ?? []));
  createFileInput.value = "";
});
wireDropZone(createDropZone, addCreateFiles);

createButton.addEventListener("click", async () => {
  if (createFiles.length === 0 || createWorking || !coreReady || coreFailed) return;

  createWorking = true;
  resetCreateOutput();
  updateCreateControls();
  setCreateStatus(`Creating a ZIP from ${createFiles.length} local files…`, "working");

  try {
    const names = createFiles.map((file) => file.name);
    const buffers = await Promise.all(createFiles.map((file) => file.arrayBuffer()));
    const archive = await createArchive(names, buffers);
    createdArchive = archive;
    createOutputCount.textContent = String(createFiles.length);
    createOutputSize.textContent = formatFileSize(archive.byteLength);
    createOutput.hidden = false;
    setAnswer(createAnswer, {
      file: "archive.zip",
      value: formatFileSize(archive.byteLength),
      note: `${createFiles.length} ${
        createFiles.length === 1 ? "file" : "files"
      } archived on this device`,
      // create_zip stores the bytes it is given verbatim; there is nothing the
      // core needs to disclose about the result.
      notices: [],
    });
    setCreateStatus(
      `archive.zip is ready — ${formatFileSize(archive.byteLength)}. Entry timestamps were normalized.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The ZIP archive could not be created.";
    setCreateStatus(message, "error");
    setAnswer(createAnswer, null);
  } finally {
    createWorking = false;
    updateCreateControls();
  }
});

createDownloadButton.addEventListener("click", () => {
  if (createdArchive) downloadBytes(createdArchive, "application/zip", "archive.zip");
});

function isArchiveEntry(value: unknown): value is ArchiveEntry {
  if (typeof value !== "object" || value === null) return false;
  const entry = value as Record<string, unknown>;
  return (
    typeof entry.name === "string" &&
    typeof entry.size === "number" &&
    Number.isFinite(entry.size) &&
    entry.size >= 0 &&
    typeof entry.compressed === "number" &&
    Number.isFinite(entry.compressed) &&
    entry.compressed >= 0 &&
    typeof entry.is_dir === "boolean" &&
    typeof entry.unsafe_path === "boolean" &&
    typeof entry.encrypted === "boolean" &&
    typeof entry.duplicate_name === "boolean" &&
    typeof entry.extractable === "boolean"
  );
}

function parseArchiveReport(report: string): ArchiveListing {
  const parsed: unknown = JSON.parse(report);
  if (typeof parsed !== "object" || parsed === null || !("entries" in parsed)) {
    throw new Error("The local core returned an invalid archive listing.");
  }
  const { entries, total_size: totalSize, warnings } = parsed as {
    entries: unknown;
    total_size: unknown;
    warnings: unknown;
  };
  if (!Array.isArray(entries) || !entries.every(isArchiveEntry)) {
    throw new Error("The local core returned an invalid archive listing.");
  }
  if (typeof totalSize !== "number" || !Number.isFinite(totalSize) || totalSize < 0) {
    throw new Error("The local core returned an invalid archive total size.");
  }
  // Rejected rather than defaulted to []: a missing warnings list would mean
  // silently telling the user an archive is fine when nothing said it was.
  if (!Array.isArray(warnings) || !warnings.every((value) => typeof value === "string")) {
    throw new Error("The local core returned an invalid archive warning list.");
  }
  return { entries, totalSize, warnings };
}

function sanitizedBasename(path: string, index: number): string {
  const segments = path
    .replace(/\\/g, "/")
    .split("/")
    .filter((segment) => segment !== "" && segment !== "." && segment !== "..");
  const basename = (segments.at(-1) ?? "")
    .replace(/[<>:"/\\|?*\u0000-\u001f]/g, "_")
    .replace(/[. ]+$/g, "")
    .trim();
  if (basename === "" || basename === "." || basename === "..") {
    return `archive-entry-${index + 1}`;
  }
  return basename.slice(0, 200);
}

const MIME_BY_EXTENSION: Record<string, string> = {
  csv: "text/csv",
  gif: "image/gif",
  html: "text/html",
  jpeg: "image/jpeg",
  jpg: "image/jpeg",
  json: "application/json",
  md: "text/markdown",
  pdf: "application/pdf",
  png: "image/png",
  svg: "image/svg+xml",
  txt: "text/plain",
  webp: "image/webp",
  xml: "application/xml",
  zip: "application/zip",
};

function guessedMime(filename: string): string {
  const dot = filename.lastIndexOf(".");
  const extension = dot >= 0 ? filename.slice(dot + 1).toLowerCase() : "";
  return MIME_BY_EXTENSION[extension] ?? "application/octet-stream";
}

let extractFile: File | null = null;
let archiveEntries: ArchiveEntry[] = [];
let archiveWarnings: string[] = [];
let extractLoading = false;
let extractWorking = false;

/*
 * Entries "Download all" will actually produce. `extractable` is part of the
 * filter, not just `unsafe_path`: an entry the core has already said it cannot
 * read would otherwise be counted, attempted, and fail mid-run.
 */
function safeDownloadableEntries(): Array<[ArchiveEntry, number]> {
  return archiveEntries
    .map((entry, index) => [entry, index] as [ArchiveEntry, number])
    .filter(([entry]) => !entry.is_dir && !entry.unsafe_path && entry.extractable);
}

/** Entries listed but deliberately left out of "Download all". */
function skippedEntryCount(): number {
  return archiveEntries.filter(
    (entry) => !entry.is_dir && (entry.unsafe_path || !entry.extractable),
  ).length;
}

function updateExtractControls() {
  const unavailable = extractLoading || extractWorking || coreFailed || !coreReady;
  extractFileInput.disabled = unavailable;
  extractAllButton.disabled = unavailable || safeDownloadableEntries().length === 0;
  extractAllButton.textContent = extractWorking ? "Extracting…" : "Download all safe files";

  for (const button of extractEntryTable.querySelectorAll<HTMLButtonElement>("button")) {
    button.disabled = unavailable;
  }
}

/** A standing label on an entry row: what is true of this entry, always. */
function addEntryFlag(container: HTMLElement, label: string, level: "alarm" | "plain") {
  const flag = document.createElement("span");
  flag.className = "entry-flag";
  if (level === "alarm") flag.dataset.level = "alarm";
  flag.textContent = label;
  container.append(flag);
}

/**
 * Why the core will not hand this entry over. `encrypted` is the one reason it
 * reports per entry; anything else that clears `extractable` is described in
 * the archive-wide warnings above the table.
 */
function blockedReason(entry: ArchiveEntry): string {
  if (entry.encrypted) return "Password-protected";
  return "Cannot be extracted";
}

function renderArchiveWarnings() {
  extractWarningList.replaceChildren();
  if (archiveWarnings.length === 0) {
    extractWarnings.hidden = true;
    return;
  }
  for (const warning of archiveWarnings) {
    const item = document.createElement("li");
    item.className = "notice";
    // textContent, never innerHTML: the sentence is the core's, shown as written.
    item.textContent = warning;
    extractWarningList.append(item);
  }
  extractWarnings.hidden = false;
}

function renderArchiveEntries() {
  extractEntryTable.replaceChildren();
  renderArchiveWarnings();

  const header = document.createElement("div");
  header.className = "organize-page";
  header.setAttribute("role", "row");
  for (const label of ["#", "Name", "Size", "Action"]) {
    const cell = document.createElement("strong");
    cell.setAttribute("role", "columnheader");
    cell.textContent = label;
    header.append(cell);
  }
  extractEntryTable.append(header);

  archiveEntries.forEach((entry, index) => {
    const row = document.createElement("div");
    row.className = "organize-page";
    row.setAttribute("role", "row");

    const order = document.createElement("span");
    order.className = "file-order";
    order.setAttribute("role", "cell");
    order.textContent = String(index + 1);

    const details = document.createElement("span");
    details.className = "file-details";
    details.setAttribute("role", "cell");
    const name = document.createElement("span");
    name.className = "file-name";
    name.textContent = entry.name;
    const compression = document.createElement("span");
    compression.className = "file-size";
    compression.textContent = entry.is_dir
      ? "Folder"
      : `${formatFileSize(entry.compressed)} compressed`;
    details.append(name, compression);
    // Two rows can legitimately share a name. Saying which ones are duplicates
    // is the difference between "this tool is broken" and "this archive is odd".
    if (entry.duplicate_name) addEntryFlag(details, "Duplicate name", "plain");
    if (entry.unsafe_path) addEntryFlag(details, "Unsafe path", "alarm");

    const size = document.createElement("span");
    size.className = "file-size";
    size.setAttribute("role", "cell");
    size.textContent = formatFileSize(entry.size);

    const action = document.createElement("span");
    action.className = "file-actions";
    action.setAttribute("role", "cell");
    if (entry.is_dir) {
      action.textContent = "Folder";
    } else if (!entry.extractable) {
      /*
       * The defect this closes: the old table offered a Download button on
       * every file row, including ones the core had already determined it
       * could not read. Pressing it produced an error and nothing else. The
       * reason belongs here, where the button used to be.
       */
      const blocked = document.createElement("span");
      blocked.className = "entry-blocked";
      blocked.textContent = blockedReason(entry);
      action.append(blocked);
    } else {
      const download = document.createElement("button");
      download.className = "secondary-action";
      download.type = "button";
      download.textContent = "Download";
      download.setAttribute("aria-label", `Download ${entry.name}`);
      download.addEventListener("click", () => void downloadSingleEntry(index));
      action.append(download);
    }

    row.append(order, details, size, action);
    extractEntryTable.append(row);
  });

  updateExtractControls();
}

async function loadExtractFile(file: File) {
  if (extractLoading || extractWorking || !coreReady || coreFailed) return;

  extractLoading = true;
  extractFile = file;
  archiveEntries = [];
  setAnswer(extractAnswer, null);
  extractEditor.hidden = true;
  extractSourceName.textContent = file.name;
  extractSourceSize.textContent = formatFileSize(file.size);
  updateExtractControls();
  setExtractStatus(`Inspecting ${file.name} locally…`, "working");

  try {
    const listing = parseArchiveReport(await listArchive(await file.arrayBuffer()));
    archiveEntries = listing.entries;
    archiveWarnings = listing.warnings;
    extractEntryCount.textContent =
      `${archiveEntries.length} ${archiveEntries.length === 1 ? "entry" : "entries"}` +
      ` · ${formatFileSize(listing.totalSize)} uncompressed`;
    extractEditor.hidden = false;
    renderArchiveEntries();
    const unextractable = archiveEntries.filter(
      (entry) => !entry.is_dir && !entry.extractable,
    ).length;
    setAnswer(extractAnswer, {
      file: file.name,
      value: String(archiveEntries.length),
      note:
        archiveEntries.length === 1
          ? "entry — read on this device"
          : "entries — read on this device",
      // Listing an archive is a read: the core produces no file here, so it
      // returns no FileResult notices. What it CAN say about this archive
      // comes back as `warnings`, rendered above the entry table.
      notices: [],
    });
    setDropLabel(extractDropZone, file.name, "Drop another .zip to open it", true);
    if (archiveEntries.length === 0) {
      setExtractStatus(`${file.name} is a valid empty archive.`, "success");
    } else if (unextractable > 0) {
      // The count belongs in the strip, not just in a row nobody scrolled to.
      setExtractStatus(
        `${file.name} — ${archiveEntries.length} ${archiveEntries.length === 1 ? "entry" : "entries"} listed, ` +
          `${unextractable} of which cannot be extracted here. See the notes above the list.`,
        "notice",
      );
    } else {
      setExtractStatus(
        `${file.name} is ready — ${archiveEntries.length} ${archiveEntries.length === 1 ? "entry" : "entries"} listed locally.`,
        "success",
      );
    }
  } catch (error) {
    archiveEntries = [];
    archiveWarnings = [];
    extractEditor.hidden = true;
    setAnswer(extractAnswer, null);
    setDropLabel(extractDropZone, "Drop a .zip here", "or choose one from your device", false);
    const message = error instanceof Error ? error.message : "The ZIP archive could not be opened.";
    setExtractStatus(message, "error");
  } finally {
    extractLoading = false;
    extractFileInput.value = "";
    updateExtractControls();
  }
}

extractFileInput.addEventListener("change", () => {
  const [file] = extractFileInput.files ?? [];
  if (file) void loadExtractFile(file);
});
wireDropZone(extractDropZone, ([file]) => {
  if (file) void loadExtractFile(file);
});

async function downloadSingleEntry(index: number) {
  const entry = archiveEntries[index];
  if (!extractFile || !entry || entry.is_dir || extractWorking) return;
  // Belt and braces: the row for a non-extractable entry has no button at all,
  // so reaching here would mean the listing and the table had drifted apart.
  if (!entry.extractable) return;

  extractWorking = true;
  updateExtractControls();
  setExtractStatus(`Extracting ${entry.name} locally…`, "working");

  try {
    const bytes = await extractArchiveEntry(await extractFile.arrayBuffer(), index);
    const filename = sanitizedBasename(entry.name, index);
    downloadBytes(bytes, guessedMime(filename), filename);
    setAnswer(extractAnswer, {
      file: filename,
      value: formatFileSize(bytes.byteLength),
      note: entry.unsafe_path
        ? "unsafe path renamed, then extracted on this device"
        : "extracted on this device",
      // extract_zip_entry returns the entry's own bytes, unaltered.
      notices: [],
    });
    setExtractStatus(
      entry.unsafe_path
        ? `${entry.name} was downloaded safely as ${filename}.`
        : `${filename} was extracted locally and is ready.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The archive entry could not be extracted.";
    setExtractStatus(message, "error");
    setAnswer(extractAnswer, null);
  } finally {
    extractWorking = false;
    updateExtractControls();
  }
}

extractAllButton.addEventListener("click", async () => {
  if (!extractFile || extractWorking) return;
  const safeEntries = safeDownloadableEntries();
  if (safeEntries.length === 0) return;

  extractWorking = true;
  updateExtractControls();

  try {
    for (const [position, [entry, index]] of safeEntries.entries()) {
      setExtractStatus(
        `Extracting safe file ${position + 1} of ${safeEntries.length}: ${entry.name}`,
        "working",
      );
      const bytes = await extractArchiveEntry(await extractFile.arrayBuffer(), index);
      const filename = sanitizedBasename(entry.name, index);
      downloadBytes(bytes, guessedMime(filename), filename);
    }
    setAnswer(extractAnswer, {
      file: extractFile.name,
      value: String(safeEntries.length),
      note:
        safeEntries.length === 1
          ? "file extracted on this device"
          : "files extracted on this device",
      notices: [],
    });
    const skipped = skippedEntryCount();
    setExtractStatus(
      `Downloaded ${safeEntries.length} safe ${safeEntries.length === 1 ? "file" : "files"}.` +
        (skipped > 0
          ? ` ${skipped} ${skipped === 1 ? "entry was" : "entries were"} skipped: unsafe paths, and entries this tool cannot extract.`
          : " Folders were skipped."),
      skipped > 0 ? "notice" : "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The archive entries could not be extracted.";
    setExtractStatus(message, "error");
    setAnswer(extractAnswer, null);
  } finally {
    extractWorking = false;
    updateExtractControls();
  }
});

type Tool = "create" | "extract";
const TOOL_TITLES: Record<Tool, string> = {
  create: "Create ZIP Archives — keeplocal",
  extract: "Extract ZIP Archives — keeplocal",
};
const toolButtons = Array.from(document.querySelectorAll<HTMLButtonElement>("[data-tool]"));
const toolPanels = Array.from(document.querySelectorAll<HTMLElement>("[data-tool-panel]"));
const toolSwitcher = requiredElement<HTMLElement>(".tool-switcher");
const openGraphTitle = requiredElement<HTMLMetaElement>('meta[property="og:title"]');
const twitterTitle = requiredElement<HTMLMetaElement>('meta[name="twitter:title"]');

function isTool(value: string | undefined): value is Tool {
  return value === "create" || value === "extract";
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
  const tool = toolFromHash() ?? "create";
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
const cspMeta = requiredElement<HTMLMetaElement>(
  'meta[http-equiv="Content-Security-Policy"]',
);
let inspectorReturnFocus: HTMLElement | null = null;

/*
 * The receipt. Every reading below is measured in THIS browser session — the
 * one thing an upload-model competitor cannot print. Written to every
 * [data-proof] slot at once so the header badge, the sidebar receipt and the
 * dialog can never disagree.
 */
function writeProof(name: string, text: string, state?: "pending" | "fail") {
  for (const slot of document.querySelectorAll<HTMLElement>(`[data-proof="${name}"]`)) {
    slot.textContent = text;
    if (state) slot.dataset.state = state;
    else delete slot.dataset.state;
  }
}

/*
 * The request count covers BOTH globals: this document and the worker that
 * actually holds the user's file. The worker's timeline is invisible from here,
 * so it is asked for its own tally over the message port. Any reading that
 * cannot be obtained is reported as unproven — never as a zero.
 */
async function updateExternalRequestProof(): Promise<void> {
  const page = readPageResourceTally();
  if (!page.measurable) {
    writeProof("external", "Unproven — this browser does not report resource timing", "fail");
    return;
  }

  const workerTally = await requestWorkerResourceTally();
  if (workerTally === null) {
    writeProof("external", "Unproven — the worker thread did not report", "fail");
    return;
  }
  if (!workerTally.measurable) {
    writeProof("external", "Unproven — the worker cannot report resource timing", "fail");
    return;
  }

  const external = page.external.length + workerTally.external.length;
  writeProof("external", String(external), external === 0 ? undefined : "fail");
}

function updateInspectorProof() {
  writeProof("external", "Checking page and worker…", "pending");
  void updateExternalRequestProof();

  const connectDirective = cspMeta.content
    .split(";")
    .map((directive) => directive.trim())
    .find((directive) => /^connect-src(?:\s|$)/i.test(directive));
  writeProof("connect-src", connectDirective ?? "Not declared", connectDirective ? undefined : "fail");

  // The directive that actually covers the Web Worker: a dedicated worker
  // inherits the owner document's policy, so this is the line that binds the
  // thread doing the file work. The request count above cannot see it.
  const workerDirective = cspMeta.content
    .split(";")
    .map((directive) => directive.trim())
    .find((directive) => /^worker-src(?:\s|$)/i.test(directive));
  writeProof("worker-src", workerDirective ?? "Not declared", workerDirective ? undefined : "fail");

  const controlled =
    "serviceWorker" in navigator && navigator.serviceWorker.controller !== null;
  writeProof(
    "sw",
    controlled ? "A service worker controls this page" : "Reload once to enable offline",
    controlled ? undefined : "pending",
  );

  writeProof(
    "stamp",
    new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" }),
  );
}

for (const button of document.querySelectorAll<HTMLButtonElement>("[data-proof-recheck]")) {
  button.addEventListener("click", () => updateInspectorProof());
}

updateInspectorProof();

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

setCreateStatus("Choose files to begin — they never leave your device.", "ready");
setExtractStatus("Choose a ZIP archive to inspect locally.", "ready");
renderCreateFiles();
renderArchiveEntries();

const themeToggle = document.querySelector<HTMLButtonElement>("#theme-toggle");
const themeLabel = document.querySelector<HTMLSpanElement>("#theme-label");
const themeColor = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');

function preferredTheme(): "light" | "dark" {
  const stored = localStorage.getItem("keeplocal-theme");
  if (stored === "light" || stored === "dark") return stored;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function applyTheme(theme: "light" | "dark") {
  document.documentElement.dataset.theme = theme;
  themeToggle?.setAttribute("aria-pressed", String(theme === "dark"));
  themeToggle?.setAttribute("aria-label", `Use ${theme === "dark" ? "light" : "dark"} theme`);
  if (themeLabel) themeLabel.textContent = theme === "dark" ? "Light" : "Dark";
  if (themeColor) themeColor.content = theme === "dark" ? "#0b120f" : "#f6f7f5";
}

applyTheme(preferredTheme());
themeToggle?.addEventListener("click", () => {
  const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  localStorage.setItem("keeplocal-theme", theme);
  applyTheme(theme);
});

if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    void navigator.serviceWorker.register("/sw.js");
  });
}
