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
  | { id: number; type: "inspect"; bytes: ArrayBuffer }
  | { id: number; type: "scrub"; bytes: ArrayBuffer }
  | { id: number; type: "verify"; bytes: ArrayBuffer };
type WorkerSuccess =
  | { type: "inspected"; id: number; report: string }
  | { type: "result"; id: number; bytes: ArrayBuffer }
  | { type: "verified"; id: number };
type WorkerResponse =
  | { type: "ready"; version: string }
  | ResourceProofResponse
  | WorkerSuccess
  | { type: "error"; id?: number; message: string };

type MetadataKind = "pdf" | "jpeg" | "png";
type MetadataItem = {
  label: string;
  detail: string | null;
  sensitive: boolean;
};
type MetadataReport = {
  kind: MetadataKind;
  items: MetadataItem[];
};
// "notice": the operation worked, but something is true of the file that the
// page's own copy would otherwise gloss over. See shared/notices.ts.
type StatusState = "ready" | "working" | "success" | "error" | "notice";

const worker = new Worker(new URL("./core.worker.ts", import.meta.url), { type: "module" });
const pending = new Map<
  number,
  { resolve: (result: WorkerSuccess) => void; reject: (reason: Error) => void }
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
  if (
    response.type === "inspected" ||
    response.type === "result" ||
    response.type === "verified"
  ) {
    pending.get(response.id)?.resolve(response);
    pending.delete(response.id);
    return;
  }
  if (response.id === undefined) {
    onCoreFailed();
    return;
  }
  pending.get(response.id)?.reject(new Error(response.message));
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
): Promise<WorkerSuccess> {
  return new Promise<WorkerSuccess>((resolve, reject) => {
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

async function inspectBytes(bytes: ArrayBuffer): Promise<string> {
  const id = nextRequestId++;
  const response = await runCoreRequest({ id, type: "inspect", bytes }, [bytes]);
  if (response.type !== "inspected") {
    throw new Error("The local core returned an unexpected inspection result.");
  }
  return response.report;
}

async function scrubBytes(bytes: ArrayBuffer): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const response = await runCoreRequest({ id, type: "scrub", bytes }, [bytes]);
  if (response.type !== "result") {
    throw new Error("The local core returned an unexpected scrub result.");
  }
  return response.bytes;
}

/**
 * Proves a cleaned file carries no metadata container, using the core's
 * structural verifier. Rejects with the core's own message naming what it
 * found; resolves only on a clean walk of the whole file.
 *
 * This deliberately does NOT re-run inspect_metadata. Asking the same detector
 * the same question a second time is a check that cannot fail for the one
 * reason that matters — a container the detector never knew how to see.
 */
async function verifyRemoved(bytes: ArrayBuffer): Promise<void> {
  const id = nextRequestId++;
  const response = await runCoreRequest({ id, type: "verify", bytes }, [bytes]);
  if (response.type !== "verified") {
    throw new Error("The local core returned an unexpected verification result.");
  }
}

/**
 * The container format, read from the file's own signature. Orchestration
 * only — no interpretation of the contents, which stays in Rust. Used to
 * confirm the scrubber handed back the same kind of file it was given.
 */
function detectKind(buffer: ArrayBuffer): MetadataKind | null {
  const bytes = new Uint8Array(buffer);
  const starts = (expected: number[]) =>
    expected.every((value, index) => bytes[index] === value);
  if (starts([0x25, 0x50, 0x44, 0x46])) return "pdf";
  if (starts([0xff, 0xd8, 0xff])) return "jpeg";
  if (starts([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return "png";
  return null;
}

function requiredElement<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) throw new Error(`Required interface element is missing: ${selector}`);
  return element;
}

const coreVersionElement = requiredElement<HTMLElement>("#core-version");
const scrubResult = requiredElement<HTMLDivElement>("#scrub-result");
const scrubResultText = requiredElement<HTMLSpanElement>("#scrub-result-text");
const scrubFileInput = requiredElement<HTMLInputElement>("#scrub-file-input");
const scrubDropZone = requiredElement<HTMLDivElement>("#scrub-drop-zone");
const scrubEditor = requiredElement<HTMLElement>("#scrub-editor");
const scrubSourceName = requiredElement<HTMLSpanElement>("#scrub-source-name");
const scrubSourceSize = requiredElement<HTMLSpanElement>("#scrub-source-size");
const scrubPreview = requiredElement<HTMLImageElement>("#scrub-preview");
const scrubPreviewDetail = requiredElement<HTMLSpanElement>("#scrub-preview-detail");
const scrubPdfChip = requiredElement<HTMLDivElement>("#scrub-pdf-chip");
const scrubPdfName = requiredElement<HTMLElement>("#scrub-pdf-name");
const scrubPdfSize = requiredElement<HTMLSpanElement>("#scrub-pdf-size");
const metadataReport = requiredElement<HTMLDivElement>("#metadata-report");
const metadataCount = requiredElement<HTMLSpanElement>("#metadata-count");
const metadataList = requiredElement<HTMLUListElement>("#metadata-list");
const metadataClean = requiredElement<HTMLDivElement>("#metadata-clean");
const scrubButton = requiredElement<HTMLButtonElement>("#scrub-button");
const scrubOutput = requiredElement<HTMLDivElement>("#scrub-output");
const scrubBeforeSize = requiredElement<HTMLElement>("#scrub-before-size");
const scrubAfterSize = requiredElement<HTMLElement>("#scrub-after-size");
const scrubRemovedCount = requiredElement<HTMLElement>("#scrub-removed-count");
const scrubRemovedSummary = requiredElement<HTMLParagraphElement>("#scrub-removed-summary");
const scrubProof = requiredElement<HTMLDivElement>("#scrub-proof");
const scrubDownloadButton = requiredElement<HTMLButtonElement>("#scrub-download-button");

type LoadedFile = {
  file: File;
  bytes: ArrayBuffer;
  report: MetadataReport;
  previewUrl: string;
  /**
   * Set when the structural verifier found a metadata container the inspector
   * did not list. The file is then NOT clean, whatever the item count says,
   * and the scrub stays available so the user can act on it.
   */
  structuralProblem: string | null;
};

type DownloadAsset = {
  url: string;
  filename: string;
};

let source: LoadedFile | null = null;
let downloadAsset: DownloadAsset | null = null;
let loading = false;
let working = false;
let scrubComplete = false;
let coreReady = false;
let coreFailed = false;

function setStatus(text: string, state: StatusState) {
  scrubResultText.textContent = text;
  scrubResult.dataset.state = state;
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

const scrubAnswer = requiredElement<HTMLElement>("#scrub-answer");

function onCoreReady(coreVersion: string) {
  coreReady = true;
  coreVersionElement.textContent = `v${coreVersion} · WebAssembly`;
  delete coreVersionElement.dataset.state;
  setStatus("Choose a PDF, JPEG, or PNG to inspect — it never leaves your device.", "ready");
  updateControls();
}

function onCoreFailed() {
  coreFailed = true;
  coreVersionElement.textContent = "Unavailable";
  setStatus("The local processing core could not start. Reload to try again.", "error");
  updateControls();
}

function updateControls() {
  const unavailable = loading || working || coreFailed || !coreReady;
  scrubFileInput.disabled = unavailable;
  scrubButton.disabled =
    unavailable ||
    source === null ||
    (source.report.items.length === 0 && source.structuralProblem === null) ||
    scrubComplete;
  scrubButton.textContent = working
    ? "Removing metadata…"
    : scrubComplete
      ? "Metadata removed"
      : "Remove all metadata";
  scrubDownloadButton.disabled = working || downloadAsset === null;
}

function wireDropZone(dropZone: HTMLDivElement, onFiles: (files: File[]) => void) {
  for (const eventName of ["dragenter", "dragover"]) {
    dropZone.addEventListener(eventName, (event) => {
      event.preventDefault();
      if (!scrubFileInput.disabled) dropZone.dataset.dragging = "true";
    });
  }
  for (const eventName of ["dragleave", "drop"]) {
    dropZone.addEventListener(eventName, (event) => {
      event.preventDefault();
      delete dropZone.dataset.dragging;
    });
  }
  dropZone.addEventListener("drop", (event) => {
    if (scrubFileInput.disabled) return;
    const files = Array.from(event.dataTransfer?.files ?? []);
    if (files.length > 0) onFiles(files);
  });
}

function formatFileSize(bytes: number): string {
  if (bytes < 1_000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1_000).toFixed(1)} KB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function parseReport(serialized: string): MetadataReport {
  let parsed: unknown;
  try {
    parsed = JSON.parse(serialized);
  } catch {
    throw new Error("The local core returned an unreadable metadata report.");
  }
  if (!isRecord(parsed) || !["pdf", "jpeg", "png"].includes(String(parsed.kind))) {
    throw new Error("The local core returned an invalid metadata report.");
  }
  if (!Array.isArray(parsed.items)) {
    throw new Error("The local core returned an invalid metadata item list.");
  }
  const items = parsed.items.map((value) => {
    if (
      !isRecord(value) ||
      typeof value.label !== "string" ||
      (value.detail !== null && typeof value.detail !== "string") ||
      typeof value.sensitive !== "boolean"
    ) {
      throw new Error("The local core returned an invalid metadata item.");
    }
    return {
      label: value.label,
      detail: value.detail,
      sensitive: value.sensitive,
    };
  });
  return { kind: parsed.kind as MetadataKind, items };
}

const FORMAT_INFO: Record<MetadataKind, { label: string; mime: string; extension: string }> = {
  pdf: { label: "PDF", mime: "application/pdf", extension: ".pdf" },
  jpeg: { label: "JPEG", mime: "image/jpeg", extension: ".jpg" },
  png: { label: "PNG", mime: "image/png", extension: ".png" },
};

function cleanFilename(file: File, kind: MetadataKind): string {
  const dot = file.name.lastIndexOf(".");
  const hasExtension = dot > 0 && dot < file.name.length - 1;
  const basename = hasExtension ? file.name.slice(0, dot) : file.name || "file";
  const extension = hasExtension ? file.name.slice(dot) : FORMAT_INFO[kind].extension;
  return `${basename}-clean${extension}`;
}

function clearDownload() {
  if (downloadAsset) URL.revokeObjectURL(downloadAsset.url);
  downloadAsset = null;
  scrubDownloadButton.disabled = true;
}

function prepareDownload(bytes: ArrayBuffer, loaded: LoadedFile) {
  clearDownload();
  const info = FORMAT_INFO[loaded.report.kind];
  const blob = new Blob([bytes], { type: info.mime });
  downloadAsset = {
    url: URL.createObjectURL(blob),
    filename: cleanFilename(loaded.file, loaded.report.kind),
  };
}

function resetOutput() {
  clearDownload();
  scrubOutput.hidden = true;
  scrubBeforeSize.textContent = "—";
  scrubAfterSize.textContent = "—";
  scrubRemovedCount.textContent = "—";
  scrubRemovedSummary.textContent = "";
  scrubProof.textContent = "";
}

function clearSource() {
  if (source) URL.revokeObjectURL(source.previewUrl);
  source = null;
  scrubComplete = false;
  scrubPreview.removeAttribute("src");
  scrubPreview.alt = "";
  scrubPreview.hidden = true;
  scrubPdfChip.hidden = true;
  scrubPreviewDetail.textContent = "";
  metadataReport.hidden = true;
  metadataList.replaceChildren();
  metadataClean.hidden = true;
  setAnswer(scrubAnswer, null);
  setDropLabel(
    scrubDropZone,
    "Drop a PDF, JPEG, or PNG here",
    "or choose one from your device",
    false,
  );
  resetOutput();
}

function sensitiveText(item: MetadataItem): string {
  return item.detail?.toLowerCase().includes("gps") ? "GPS location" : "Sensitive";
}

function renderMetadata(report: MetadataReport) {
  metadataReport.hidden = false;
  metadataList.replaceChildren();
  metadataCount.textContent = `${report.items.length} ${report.items.length === 1 ? "item" : "items"}`;
  metadataList.hidden = report.items.length === 0;
  metadataClean.hidden = report.items.length !== 0;

  report.items.forEach((item, index) => {
    const row = document.createElement("li");
    row.className = "merge-file";
    if (item.sensitive) {
      row.style.borderColor = "var(--alarm)";
    }

    const marker = document.createElement("span");
    marker.className = "file-order";
    marker.textContent = item.sensitive ? "!" : String(index + 1);
    if (item.sensitive) {
      marker.style.color = "var(--alarm)";
      marker.style.fontWeight = "900";
    }

    const details = document.createElement("span");
    details.className = "file-details";
    const label = document.createElement("strong");
    label.className = "file-name";
    label.textContent = item.label;
    const detail = document.createElement("span");
    detail.className = "file-size";
    detail.textContent = item.detail ?? "Stored in the file";
    details.append(label, detail);
    row.append(marker, details);

    if (item.sensitive) {
      const warning = document.createElement("span");
      warning.className = "result";
      warning.dataset.state = "error";
      warning.style.margin = "0";
      warning.style.minHeight = "34px";
      warning.style.padding = "5px 9px";
      warning.style.fontSize = "0.72rem";
      const dot = document.createElement("span");
      dot.className = "status-dot";
      dot.setAttribute("aria-hidden", "true");
      const warningText = document.createElement("span");
      warningText.textContent = sensitiveText(item);
      warning.append(dot, warningText);
      row.append(warning);
    }
    metadataList.append(row);
  });
}

async function showSourcePresentation(loaded: LoadedFile) {
  const { file, report, previewUrl } = loaded;
  if (report.kind === "pdf") {
    scrubPreview.hidden = true;
    scrubPdfName.textContent = file.name;
    scrubPdfSize.textContent = `${FORMAT_INFO.pdf.label} · ${formatFileSize(file.size)}`;
    scrubPdfChip.hidden = false;
    scrubPreviewDetail.textContent = "Document content stays intact; page count is verified after cleaning.";
    return;
  }

  scrubPdfChip.hidden = true;
  scrubPreview.src = previewUrl;
  scrubPreview.alt = `Preview of ${file.name}`;
  try {
    await scrubPreview.decode();
    scrubPreview.hidden = false;
    scrubPreviewDetail.textContent =
      `${FORMAT_INFO[report.kind].label} preview · ${scrubPreview.naturalWidth} × ${scrubPreview.naturalHeight}px`;
  } catch {
    scrubPreview.hidden = true;
    scrubPreviewDetail.textContent = "Preview unavailable; the local core still validated the image bytes.";
  }
}

function showAlreadyCleanOutput(loaded: LoadedFile) {
  prepareDownload(loaded.bytes.slice(0), loaded);
  scrubBeforeSize.textContent = formatFileSize(loaded.file.size);
  scrubAfterSize.textContent = formatFileSize(loaded.file.size);
  scrubRemovedCount.textContent = "0 items";
  scrubRemovedSummary.textContent = "Nothing needed removal; the download is the original clean file.";
  scrubProof.textContent =
    "Structural check passed: every byte of this file was walked and no metadata container was found.";
  scrubOutput.hidden = false;
}

async function processFile(file: File) {
  if (loading || working || coreFailed || !coreReady) return;

  clearSource();
  loading = true;
  updateControls();
  scrubEditor.hidden = false;
  scrubSourceName.textContent = file.name;
  scrubSourceSize.textContent = formatFileSize(file.size);
  scrubPreviewDetail.textContent = "Inspecting file bytes in this tab…";
  setStatus(`Inspecting ${file.name} locally…`, "working");
  const previewUrl = URL.createObjectURL(file);

  try {
    const bytes = await file.arrayBuffer();
    const report = parseReport(await inspectBytes(bytes.slice(0)));
    /*
     * "Nothing found" from the inspector is the weakest claim this tool can
     * make, because it is the same detector that would be blind to a container
     * it does not know about. Before telling anyone their file is already
     * clean, walk its structure independently.
     */
    let structuralProblem: string | null = null;
    if (report.items.length === 0) {
      try {
        await verifyRemoved(bytes.slice(0));
      } catch (error) {
        structuralProblem =
          error instanceof Error ? error.message : "This file still carries metadata.";
      }
    }
    const loaded: LoadedFile = { file, bytes, report, previewUrl, structuralProblem };
    source = loaded;
    renderMetadata(report);
    await showSourcePresentation(loaded);
    setDropLabel(scrubDropZone, file.name, "Drop another file to inspect it", true);

    if (report.items.length === 0 && structuralProblem === null) {
      showAlreadyCleanOutput(loaded);
      setAnswer(scrubAnswer, {
        file: file.name,
        value: "0",
        note: "metadata items found — verified clean on this device",
        // scrub_metadata does not transform bytes it was not asked to, so the
        // core has no FileResult notices to attach here.
        notices: [],
      });
      setStatus("No metadata found — this file is already clean and ready to download.", "success");
    } else if (report.items.length === 0) {
      // Listed nothing, but the structural walk disagrees. Say the harder thing.
      setAnswer(scrubAnswer, {
        file: file.name,
        value: "0",
        note: "items listed, but the structural check disagrees — see below",
        notices: [
          {
            code: "scrub-structural-check-failed",
            message: `The metadata list is empty, but a structural check of this file still found something: ${structuralProblem} Removing metadata is worth running anyway.`,
          },
        ],
      });
      setStatus(structuralProblem ?? "This file still carries metadata.", "notice");
    } else {
      const sensitiveCount = report.items.filter((item) => item.sensitive).length;
      const warning = sensitiveCount > 0
        ? ` ${sensitiveCount} ${sensitiveCount === 1 ? "item is" : "items are"} flagged as sensitive.`
        : "";
      setAnswer(scrubAnswer, {
        file: file.name,
        value: String(report.items.length),
        note: `${report.items.length === 1 ? "metadata item" : "metadata items"} found${
          sensitiveCount > 0 ? `, ${sensitiveCount} sensitive` : ""
        } — inspected on this device`,
        // Inspecting produces no file, so the core has nothing to disclose.
        notices: [],
      });
      setStatus(
        `${report.items.length} metadata ${report.items.length === 1 ? "item" : "items"} found.${warning}`,
        "ready",
      );
    }
  } catch (error) {
    URL.revokeObjectURL(previewUrl);
    source = null;
    scrubEditor.hidden = true;
    setAnswer(scrubAnswer, null);
    setDropLabel(
      scrubDropZone,
      "Drop a PDF, JPEG, or PNG here",
      "or choose one from your device",
      false,
    );
    const message = error instanceof Error ? error.message : "This file could not be inspected.";
    setStatus(message, "error");
  } finally {
    loading = false;
    scrubFileInput.value = "";
    updateControls();
  }
}

function removedLabels(items: MetadataItem[]): string[] {
  return Array.from(
    new Set(
      items.map((item) =>
        item.label === "EXIF" && item.detail?.toLowerCase().includes("gps")
          ? "EXIF (GPS)"
          : item.label,
      ),
    ),
  );
}

scrubFileInput.addEventListener("change", () => {
  const [file] = scrubFileInput.files ?? [];
  if (file) void processFile(file);
});

wireDropZone(scrubDropZone, ([file]) => {
  if (file) void processFile(file);
});

scrubButton.addEventListener("click", async () => {
  if (!source || source.report.items.length === 0 || working || coreFailed) return;
  const loaded = source;
  working = true;
  scrubComplete = false;
  resetOutput();
  updateControls();
  setStatus(`Removing metadata from ${loaded.file.name} locally…`, "working");

  try {
    const scrubbed = await scrubBytes(loaded.bytes.slice(0));
    if (detectKind(scrubbed) !== loaded.report.kind) {
      throw new Error("The cleaned file is no longer the format you gave us, so it was rejected.");
    }
    // Throws with the core's own message naming the container it found.
    await verifyRemoved(scrubbed.slice(0));

    prepareDownload(scrubbed, loaded);
    scrubBeforeSize.textContent = formatFileSize(loaded.file.size);
    scrubAfterSize.textContent = formatFileSize(scrubbed.byteLength);
    scrubRemovedCount.textContent =
      `${loaded.report.items.length} ${loaded.report.items.length === 1 ? "item" : "items"}`;
    scrubRemovedSummary.textContent = `Removed: ${removedLabels(loaded.report.items).join(", ")}.`;
    scrubProof.textContent =
      "Structural check passed: every byte of the cleaned file was walked and no metadata container was found.";
    scrubOutput.hidden = false;
    scrubComplete = true;
    setAnswer(scrubAnswer, {
      file: downloadAsset?.filename ?? cleanFilename(loaded.file, loaded.report.kind),
      value: String(loaded.report.items.length),
      note: `${
        loaded.report.items.length === 1 ? "metadata item" : "metadata items"
      } removed — cleaned on this device`,
      // scrub_metadata removes only qualified containers and transforms
      // nothing else, so the core attaches no notices to this result.
      notices: [],
    });
    setStatus(
      `Metadata removed. The cleaned ${FORMAT_INFO[loaded.report.kind].label} is ready to download.`,
      "success",
    );
  } catch (error) {
    setAnswer(scrubAnswer, null);
    const message = error instanceof Error ? error.message : "The file could not be cleaned.";
    setStatus(message, "error");
  } finally {
    working = false;
    updateControls();
  }
});

scrubDownloadButton.addEventListener("click", () => {
  if (!downloadAsset) return;
  const link = document.createElement("a");
  link.href = downloadAsset.url;
  link.download = downloadAsset.filename;
  document.body.append(link);
  link.click();
  link.remove();
});

type Tool = "scrub";
const TOOL_TITLE = "Remove File Metadata — keeplocal";
const toolButton = requiredElement<HTMLButtonElement>('[data-tool="scrub"]');
const toolPanel = requiredElement<HTMLElement>('[data-tool-panel="scrub"]');
const toolSwitcher = requiredElement<HTMLElement>(".tool-switcher");
const openGraphTitle = requiredElement<HTMLMetaElement>('meta[property="og:title"]');
const twitterTitle = requiredElement<HTMLMetaElement>('meta[name="twitter:title"]');

function isTool(value: string | undefined): value is Tool {
  return value === "scrub";
}

function switchTool(tool: Tool, updateHash = false) {
  toolButton.tabIndex = 0;
  toolButton.setAttribute("aria-current", "page");
  toolPanel.hidden = tool !== "scrub";
  document.title = TOOL_TITLE;
  openGraphTitle.content = TOOL_TITLE;
  twitterTitle.content = TOOL_TITLE;
  if (updateHash && location.hash !== `#${tool}`) {
    history.pushState(null, "", `${location.pathname}${location.search}#${tool}`);
  }
}

toolButton.addEventListener("click", () => switchTool("scrub", true));
toolSwitcher.addEventListener("keydown", (event) => {
  if (!["ArrowRight", "ArrowDown", "ArrowLeft", "ArrowUp", "Home", "End"].includes(event.key)) {
    return;
  }
  event.preventDefault();
  switchTool("scrub", true);
  toolButton.focus();
});

function restoreToolFromHash() {
  const hashTool = location.hash.slice(1);
  switchTool(isTool(hashTool) ? hashTool : "scrub");
  if (location.hash !== "#scrub") {
    history.replaceState(null, "", `${location.pathname}${location.search}#scrub`);
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

setStatus("Loading the local processing core…", "working");
updateControls();

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

window.addEventListener("beforeunload", () => {
  if (source) URL.revokeObjectURL(source.previewUrl);
  if (downloadAsset) URL.revokeObjectURL(downloadAsset.url);
});

if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    void navigator.serviceWorker.register("/sw.js");
  });
}
