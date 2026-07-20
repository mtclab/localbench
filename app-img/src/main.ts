import "./style.css";

type ConvertTarget = "png" | "jpeg" | "webp";

type WorkerRequest =
  | { id: number; type: "resize"; bytes: ArrayBuffer; maxW: number; maxH: number; keepAspect: boolean }
  | { id: number; type: "convert"; bytes: ArrayBuffer; target: ConvertTarget }
  | { id: number; type: "compress"; bytes: ArrayBuffer; quality: number };

type WorkerResult = { type: "result"; id: number; bytes: ArrayBuffer };

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

async function resizeImage(
  bytes: ArrayBuffer,
  maxW: number,
  maxH: number,
  keepAspect: boolean,
): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest(
    { id, type: "resize", bytes, maxW, maxH, keepAspect },
    [bytes],
  );
  return result.bytes;
}

async function convertImage(bytes: ArrayBuffer, target: ConvertTarget): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "convert", bytes, target }, [bytes]);
  return result.bytes;
}

async function compressImage(bytes: ArrayBuffer, quality: number): Promise<ArrayBuffer> {
  const id = nextRequestId++;
  const result = await runCoreRequest({ id, type: "compress", bytes, quality }, [bytes]);
  return result.bytes;
}

function requiredElement<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) throw new Error(`Required interface element is missing: ${selector}`);
  return element;
}

const resizeResult = requiredElement<HTMLDivElement>("#resize-result");
const resizeResultText = requiredElement<HTMLSpanElement>("#resize-result-text");
const resizeFileInput = requiredElement<HTMLInputElement>("#resize-file-input");
const resizeDropZone = requiredElement<HTMLDivElement>("#resize-drop-zone");
const resizeEditor = requiredElement<HTMLElement>("#resize-editor");
const resizeSourceName = requiredElement<HTMLSpanElement>("#resize-source-name");
const resizeSourceSize = requiredElement<HTMLSpanElement>("#resize-source-size");
const resizePreview = requiredElement<HTMLImageElement>("#resize-preview");
const resizePreviewDetail = requiredElement<HTMLSpanElement>("#resize-preview-detail");
const resizeMaxWidth = requiredElement<HTMLInputElement>("#resize-max-width");
const resizeMaxHeight = requiredElement<HTMLInputElement>("#resize-max-height");
const resizeKeepAspect = requiredElement<HTMLInputElement>("#resize-keep-aspect");
const resizeButton = requiredElement<HTMLButtonElement>("#resize-button");
const resizeOutput = requiredElement<HTMLDivElement>("#resize-output");
const resizeOutputDimensions = requiredElement<HTMLElement>("#resize-output-dimensions");
const resizeOutputSize = requiredElement<HTMLElement>("#resize-output-size");
const resizeDownloadButton = requiredElement<HTMLButtonElement>("#resize-download-button");

const convertResult = requiredElement<HTMLDivElement>("#convert-result");
const convertResultText = requiredElement<HTMLSpanElement>("#convert-result-text");
const convertFileInput = requiredElement<HTMLInputElement>("#convert-file-input");
const convertDropZone = requiredElement<HTMLDivElement>("#convert-drop-zone");
const convertEditor = requiredElement<HTMLElement>("#convert-editor");
const convertSourceName = requiredElement<HTMLSpanElement>("#convert-source-name");
const convertSourceSize = requiredElement<HTMLSpanElement>("#convert-source-size");
const convertPreview = requiredElement<HTMLImageElement>("#convert-preview");
const convertPreviewDetail = requiredElement<HTMLSpanElement>("#convert-preview-detail");
const convertTarget = requiredElement<HTMLSelectElement>("#convert-target");
const convertJpegNote = requiredElement<HTMLParagraphElement>("#convert-jpeg-note");
const convertButton = requiredElement<HTMLButtonElement>("#convert-button");
const convertOutput = requiredElement<HTMLDivElement>("#convert-output");
const convertOutputFormat = requiredElement<HTMLElement>("#convert-output-format");
const convertOutputDimensions = requiredElement<HTMLElement>("#convert-output-dimensions");
const convertOutputSize = requiredElement<HTMLElement>("#convert-output-size");
const convertDownloadButton = requiredElement<HTMLButtonElement>("#convert-download-button");

const compressResult = requiredElement<HTMLDivElement>("#compress-result");
const compressResultText = requiredElement<HTMLSpanElement>("#compress-result-text");
const compressFileInput = requiredElement<HTMLInputElement>("#compress-file-input");
const compressDropZone = requiredElement<HTMLDivElement>("#compress-drop-zone");
const compressEditor = requiredElement<HTMLElement>("#compress-editor");
const compressSourceName = requiredElement<HTMLSpanElement>("#compress-source-name");
const compressSourceSize = requiredElement<HTMLSpanElement>("#compress-source-size");
const compressPreview = requiredElement<HTMLImageElement>("#compress-preview");
const compressPreviewDetail = requiredElement<HTMLSpanElement>("#compress-preview-detail");
const compressQuality = requiredElement<HTMLInputElement>("#compress-quality");
const compressQualityValue = requiredElement<HTMLOutputElement>("#compress-quality-value");
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

function setResizeStatus(text: string, state: StatusState) {
  setStatus(resizeResult, resizeResultText, text, state);
}

function setConvertStatus(text: string, state: StatusState) {
  setStatus(convertResult, convertResultText, text, state);
}

function setCompressStatus(text: string, state: StatusState) {
  setStatus(compressResult, compressResultText, text, state);
}

let coreFailed = false;

function onCoreReady(coreVersion: string) {
  coreFailed = false;
  version.textContent = `v${coreVersion}`;
  updateResizeControls();
  updateConvertControls();
  updateCompressControls();
}

function onCoreFailed() {
  coreFailed = true;
  version.textContent = "Unavailable";
  setResizeStatus("The local processing core could not load.", "error");
  setConvertStatus("The local processing core could not load.", "error");
  setCompressStatus("The local processing core could not load.", "error");
  updateResizeControls();
  updateConvertControls();
  updateCompressControls();
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
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

type Dimensions = { width: number; height: number };

function formatDimensions(dimensions: Dimensions): string {
  return `${dimensions.width} × ${dimensions.height} px`;
}

function loadPreviewSource(
  image: HTMLImageElement,
  url: string,
  alt: string,
): Promise<Dimensions | null> {
  return new Promise((resolve) => {
    const cleanup = () => {
      image.removeEventListener("load", onLoad);
      image.removeEventListener("error", onError);
    };
    const onLoad = () => {
      cleanup();
      image.hidden = false;
      resolve({ width: image.naturalWidth, height: image.naturalHeight });
    };
    const onError = () => {
      cleanup();
      image.hidden = true;
      resolve(null);
    };

    image.addEventListener("load", onLoad);
    image.addEventListener("error", onError);
    image.alt = alt;
    image.src = url;
  });
}

function blobDataUrl(blob: Blob): Promise<string | null> {
  return new Promise((resolve) => {
    const reader = new FileReader();
    reader.addEventListener("load", () => {
      resolve(typeof reader.result === "string" ? reader.result : null);
    });
    reader.addEventListener("error", () => resolve(null));
    reader.readAsDataURL(blob);
  });
}

async function showPreview(
  image: HTMLImageElement,
  objectUrl: string,
  alt: string,
  blob: Blob,
): Promise<{ dimensions: Dimensions | null; displayUrl: string }> {
  const objectUrlDimensions = await loadPreviewSource(image, objectUrl, alt);
  if (objectUrlDimensions) {
    return { dimensions: objectUrlDimensions, displayUrl: objectUrl };
  }

  // The copied strict CSP intentionally allows self/data images but not blob:
  // in browsers that don't treat blob URLs as self, keep the preview local by
  // falling back to the already-allowed data form of the same in-memory bytes.
  const dataUrl = await blobDataUrl(blob);
  if (!dataUrl) return { dimensions: null, displayUrl: objectUrl };
  return {
    dimensions: await loadPreviewSource(image, dataUrl, alt),
    displayUrl: dataUrl,
  };
}

function clearPreview(image: HTMLImageElement) {
  image.removeAttribute("src");
  image.alt = "";
  image.hidden = true;
}

type EncodedFormat = "png" | "jpeg" | "webp" | "gif" | "bmp";

const FORMAT_INFO: Record<
  EncodedFormat,
  { extension: string; mime: string; label: string }
> = {
  png: { extension: "png", mime: "image/png", label: "PNG" },
  jpeg: { extension: "jpg", mime: "image/jpeg", label: "JPEG" },
  webp: { extension: "webp", mime: "image/webp", label: "WebP" },
  gif: { extension: "gif", mime: "image/gif", label: "GIF" },
  bmp: { extension: "bmp", mime: "image/bmp", label: "BMP" },
};

function bytesMatch(bytes: Uint8Array, offset: number, expected: number[]): boolean {
  return expected.every((value, index) => bytes[offset + index] === value);
}

function detectEncodedFormat(buffer: ArrayBuffer): EncodedFormat {
  const bytes = new Uint8Array(buffer);
  if (bytesMatch(bytes, 0, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return "png";
  if (bytesMatch(bytes, 0, [0xff, 0xd8, 0xff])) return "jpeg";
  if (
    bytesMatch(bytes, 0, [0x47, 0x49, 0x46, 0x38]) &&
    (bytes[4] === 0x37 || bytes[4] === 0x39) &&
    bytes[5] === 0x61
  ) {
    return "gif";
  }
  if (bytesMatch(bytes, 0, [0x42, 0x4d])) return "bmp";
  if (
    bytesMatch(bytes, 0, [0x52, 0x49, 0x46, 0x46]) &&
    bytesMatch(bytes, 8, [0x57, 0x45, 0x42, 0x50])
  ) {
    return "webp";
  }
  throw new Error("The local core returned an image in an unexpected format.");
}

function baseName(filename: string): string {
  return filename.replace(/\.[^./]+$/, "") || "image";
}

function outputFilename(file: File, suffix: string, format: EncodedFormat): string {
  return `${baseName(file.name)}-${suffix}.${FORMAT_INFO[format].extension}`;
}

function downloadImage(bytes: ArrayBuffer, mime: string, filename: string) {
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

type LoadedImage = {
  file: File;
  previewUrl: string;
  displayUrl: string;
  dimensions: Dimensions | null;
};

type ProcessedImage = {
  bytes: ArrayBuffer;
  filename: string;
  mime: string;
  previewUrl: string;
  displayUrl: string;
  dimensions: Dimensions | null;
};

let resizeSource: LoadedImage | null = null;
let resizeProcessed: ProcessedImage | null = null;
let resizeLoading = false;
let resizeWorking = false;

function updateResizeControls() {
  const unavailable = resizeLoading || resizeWorking || coreFailed;
  resizeFileInput.disabled = unavailable;
  resizeMaxWidth.disabled = unavailable || resizeSource === null;
  resizeMaxHeight.disabled = unavailable || resizeSource === null;
  resizeKeepAspect.disabled = unavailable || resizeSource === null;
  resizeButton.disabled = unavailable || resizeSource === null;
  resizeButton.textContent = resizeWorking ? "Resizing…" : "Resize";
  resizeDownloadButton.disabled = resizeWorking || resizeProcessed === null;
}

function showResizeSourcePreview() {
  if (!resizeSource) {
    clearPreview(resizePreview);
    resizePreviewDetail.textContent = "";
    return;
  }
  resizePreview.src = resizeSource.displayUrl;
  resizePreview.alt = `Preview of ${resizeSource.file.name}`;
  resizePreview.hidden = resizeSource.dimensions === null;
  resizePreviewDetail.textContent = resizeSource.dimensions
    ? `Source preview · ${formatDimensions(resizeSource.dimensions)}`
    : "Preview unavailable; the local core will still validate the image bytes.";
}

function resetResizeOutput(restoreSource = true) {
  if (resizeProcessed) URL.revokeObjectURL(resizeProcessed.previewUrl);
  resizeProcessed = null;
  resizeOutput.hidden = true;
  resizeOutputDimensions.textContent = "—";
  resizeOutputSize.textContent = "—";
  if (restoreSource) showResizeSourcePreview();
  updateResizeControls();
}

async function processResizeFile(file: File) {
  if (resizeLoading || resizeWorking || coreFailed) return;

  resizeLoading = true;
  updateResizeControls();
  resetResizeOutput(false);
  clearPreview(resizePreview);
  if (resizeSource) URL.revokeObjectURL(resizeSource.previewUrl);

  const previewUrl = URL.createObjectURL(file);
  resizeSource = { file, previewUrl, displayUrl: previewUrl, dimensions: null };
  resizeEditor.hidden = false;
  resizeSourceName.textContent = file.name;
  resizeSourceSize.textContent = formatFileSize(file.size);
  resizePreviewDetail.textContent = "Loading local preview…";
  setResizeStatus(`Reading ${file.name} locally…`, "working");

  try {
    const preview = await showPreview(resizePreview, previewUrl, `Preview of ${file.name}`, file);
    resizeSource.dimensions = preview.dimensions;
    resizeSource.displayUrl = preview.displayUrl;
    resizePreviewDetail.textContent = preview.dimensions
      ? `Source preview · ${formatDimensions(preview.dimensions)}`
      : "Preview unavailable; the local core will still validate the image bytes.";
    setResizeStatus(
      preview.dimensions
        ? `${file.name} is ready — ${formatDimensions(preview.dimensions)}, ${formatFileSize(file.size)}.`
        : `${file.name} is ready for local validation.`,
      "ready",
    );
  } finally {
    resizeLoading = false;
    resizeFileInput.value = "";
    updateResizeControls();
  }
}

resizeFileInput.addEventListener("change", () => {
  const [file] = resizeFileInput.files ?? [];
  if (file) void processResizeFile(file);
});
wireDropZone(resizeDropZone, ([file]) => {
  if (file) void processResizeFile(file);
});

for (const control of [resizeMaxWidth, resizeMaxHeight, resizeKeepAspect]) {
  control.addEventListener("change", () => {
    if (!resizeSource || resizeWorking) return;
    resetResizeOutput();
    setResizeStatus("Resize settings updated. Ready to process locally.", "ready");
  });
}

function positiveU32(input: HTMLInputElement, label: string): number {
  const value = Number(input.value);
  if (!Number.isInteger(value) || value < 1 || value > 0xffff_ffff) {
    throw new Error(`${label} must be a whole number greater than zero.`);
  }
  return value;
}

resizeButton.addEventListener("click", async () => {
  if (!resizeSource || resizeWorking || coreFailed) return;

  let maxW: number;
  let maxH: number;
  try {
    maxW = positiveU32(resizeMaxWidth, "Max width");
    maxH = positiveU32(resizeMaxHeight, "Max height");
  } catch (error) {
    setResizeStatus(error instanceof Error ? error.message : "Choose valid resize dimensions.", "error");
    return;
  }

  resizeWorking = true;
  resetResizeOutput();
  updateResizeControls();
  setResizeStatus(`Resizing ${resizeSource.file.name} locally…`, "working");

  try {
    const resized = await resizeImage(
      await resizeSource.file.arrayBuffer(),
      maxW,
      maxH,
      resizeKeepAspect.checked,
    );
    const format = detectEncodedFormat(resized);
    const info = FORMAT_INFO[format];
    const filename = outputFilename(resizeSource.file, "resized", format);
    const previewBlob = new Blob([resized], { type: info.mime });
    const previewUrl = URL.createObjectURL(previewBlob);
    const preview = await showPreview(
      resizePreview,
      previewUrl,
      `Resized preview of ${resizeSource.file.name}`,
      previewBlob,
    );
    const dimensions = preview.dimensions;

    resizeProcessed = {
      bytes: resized,
      filename,
      mime: info.mime,
      previewUrl,
      displayUrl: preview.displayUrl,
      dimensions,
    };
    resizeOutputDimensions.textContent = dimensions ? formatDimensions(dimensions) : "Preview unavailable";
    resizeOutputSize.textContent = formatFileSize(resized.byteLength);
    resizeOutput.hidden = false;
    resizePreviewDetail.textContent = dimensions
      ? `Resized preview · ${formatDimensions(dimensions)}`
      : "The resized file is ready, but this browser could not preview it.";

    const keptOriginalSize =
      resizeSource.dimensions !== null &&
      dimensions !== null &&
      resizeSource.dimensions.width === dimensions.width &&
      resizeSource.dimensions.height === dimensions.height;
    const outcome = keptOriginalSize
      ? `The image was kept at its original size (${formatDimensions(dimensions)})`
      : dimensions
        ? `Resized to ${formatDimensions(dimensions)}`
        : "The resized image is ready";
    setResizeStatus(
      `${outcome} — ${formatFileSize(resized.byteLength)}. Location/EXIF metadata was removed.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The image could not be resized.";
    setResizeStatus(message, "error");
  } finally {
    resizeWorking = false;
    updateResizeControls();
  }
});

resizeDownloadButton.addEventListener("click", () => {
  if (resizeProcessed) {
    downloadImage(resizeProcessed.bytes, resizeProcessed.mime, resizeProcessed.filename);
  }
});

let convertSource: LoadedImage | null = null;
let convertProcessed: ProcessedImage | null = null;
let convertLoading = false;
let convertWorking = false;

function isConvertTarget(value: string): value is ConvertTarget {
  return value === "png" || value === "jpeg" || value === "webp";
}

function updateConvertControls() {
  const unavailable = convertLoading || convertWorking || coreFailed;
  convertFileInput.disabled = unavailable;
  convertTarget.disabled = unavailable || convertSource === null;
  convertButton.disabled = unavailable || convertSource === null;
  convertButton.textContent = convertWorking ? "Converting…" : "Convert";
  convertDownloadButton.disabled = convertWorking || convertProcessed === null;
}

function showConvertSourcePreview() {
  if (!convertSource) {
    clearPreview(convertPreview);
    convertPreviewDetail.textContent = "";
    return;
  }
  convertPreview.src = convertSource.displayUrl;
  convertPreview.alt = `Preview of ${convertSource.file.name}`;
  convertPreview.hidden = convertSource.dimensions === null;
  convertPreviewDetail.textContent = convertSource.dimensions
    ? `Source preview · ${formatDimensions(convertSource.dimensions)}`
    : "Preview unavailable; the local core will still validate the image bytes.";
}

function resetConvertOutput(restoreSource = true) {
  if (convertProcessed) URL.revokeObjectURL(convertProcessed.previewUrl);
  convertProcessed = null;
  convertOutput.hidden = true;
  convertOutputFormat.textContent = "—";
  convertOutputDimensions.textContent = "—";
  convertOutputSize.textContent = "—";
  if (restoreSource) showConvertSourcePreview();
  updateConvertControls();
}

function updateJpegNote() {
  convertJpegNote.hidden = convertTarget.value !== "jpeg";
}

async function processConvertFile(file: File) {
  if (convertLoading || convertWorking || coreFailed) return;

  convertLoading = true;
  updateConvertControls();
  resetConvertOutput(false);
  clearPreview(convertPreview);
  if (convertSource) URL.revokeObjectURL(convertSource.previewUrl);

  const previewUrl = URL.createObjectURL(file);
  convertSource = { file, previewUrl, displayUrl: previewUrl, dimensions: null };
  convertEditor.hidden = false;
  convertSourceName.textContent = file.name;
  convertSourceSize.textContent = formatFileSize(file.size);
  convertPreviewDetail.textContent = "Loading local preview…";
  setConvertStatus(`Reading ${file.name} locally…`, "working");

  try {
    const preview = await showPreview(convertPreview, previewUrl, `Preview of ${file.name}`, file);
    convertSource.dimensions = preview.dimensions;
    convertSource.displayUrl = preview.displayUrl;
    convertPreviewDetail.textContent = preview.dimensions
      ? `Source preview · ${formatDimensions(preview.dimensions)}`
      : "Preview unavailable; the local core will still validate the image bytes.";
    setConvertStatus(
      preview.dimensions
        ? `${file.name} is ready — ${formatDimensions(preview.dimensions)}, ${formatFileSize(file.size)}.`
        : `${file.name} is ready for local validation.`,
      "ready",
    );
  } finally {
    convertLoading = false;
    convertFileInput.value = "";
    updateConvertControls();
  }
}

convertFileInput.addEventListener("change", () => {
  const [file] = convertFileInput.files ?? [];
  if (file) void processConvertFile(file);
});
wireDropZone(convertDropZone, ([file]) => {
  if (file) void processConvertFile(file);
});

convertTarget.addEventListener("change", () => {
  updateJpegNote();
  if (!convertSource || convertWorking) return;
  resetConvertOutput();
  setConvertStatus("Target format updated. Ready to convert locally.", "ready");
});

convertButton.addEventListener("click", async () => {
  if (!convertSource || convertWorking || coreFailed) return;
  if (!isConvertTarget(convertTarget.value)) {
    setConvertStatus("Choose PNG, JPEG, or WebP as the target format.", "error");
    return;
  }
  const target = convertTarget.value;

  convertWorking = true;
  resetConvertOutput();
  updateConvertControls();
  setConvertStatus(`Converting ${convertSource.file.name} locally…`, "working");

  try {
    const converted = await convertImage(await convertSource.file.arrayBuffer(), target);
    const format = detectEncodedFormat(converted);
    if (format !== target) throw new Error("The local core returned an unexpected target format.");
    const info = FORMAT_INFO[format];
    const filename = outputFilename(convertSource.file, "converted", format);
    const previewBlob = new Blob([converted], { type: info.mime });
    const previewUrl = URL.createObjectURL(previewBlob);
    const preview = await showPreview(
      convertPreview,
      previewUrl,
      `Converted preview of ${convertSource.file.name}`,
      previewBlob,
    );
    const dimensions = preview.dimensions;

    convertProcessed = {
      bytes: converted,
      filename,
      mime: info.mime,
      previewUrl,
      displayUrl: preview.displayUrl,
      dimensions,
    };
    convertOutputFormat.textContent = info.label;
    convertOutputDimensions.textContent = dimensions ? formatDimensions(dimensions) : "Preview unavailable";
    convertOutputSize.textContent = formatFileSize(converted.byteLength);
    convertOutput.hidden = false;
    convertPreviewDetail.textContent = dimensions
      ? `Converted ${info.label} preview · ${formatDimensions(dimensions)}`
      : `The converted ${info.label} is ready, but this browser could not preview it.`;
    setConvertStatus(
      `${info.label} ready — ${formatFileSize(converted.byteLength)}. Location/EXIF metadata was removed.`,
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The image could not be converted.";
    setConvertStatus(message, "error");
  } finally {
    convertWorking = false;
    updateConvertControls();
  }
});

convertDownloadButton.addEventListener("click", () => {
  if (convertProcessed) {
    downloadImage(convertProcessed.bytes, convertProcessed.mime, convertProcessed.filename);
  }
});

let compressSource: LoadedImage | null = null;
let compressedImage: ProcessedImage | null = null;
let compressLoading = false;
let compressWorking = false;

function updateCompressControls() {
  const unavailable = compressLoading || compressWorking || coreFailed;
  compressFileInput.disabled = unavailable;
  compressQuality.disabled = unavailable || compressSource === null;
  compressButton.disabled = unavailable || compressSource === null;
  compressButton.textContent = compressWorking ? "Compressing…" : "Compress";
  compressDownloadButton.disabled = compressWorking || compressedImage === null;
}

function showCompressSourcePreview() {
  if (!compressSource) {
    clearPreview(compressPreview);
    compressPreviewDetail.textContent = "";
    return;
  }
  compressPreview.src = compressSource.displayUrl;
  compressPreview.alt = `Preview of ${compressSource.file.name}`;
  compressPreview.hidden = compressSource.dimensions === null;
  compressPreviewDetail.textContent = compressSource.dimensions
    ? `Source preview · ${formatDimensions(compressSource.dimensions)}`
    : "Preview unavailable; the local core will still validate the image bytes.";
}

function resetCompressOutput(restoreSource = true) {
  if (compressedImage) URL.revokeObjectURL(compressedImage.previewUrl);
  compressedImage = null;
  compressOutput.hidden = true;
  compressBeforeSize.textContent = "—";
  compressAfterSize.textContent = "—";
  compressSavedPercent.textContent = "—";
  if (restoreSource) showCompressSourcePreview();
  updateCompressControls();
}

async function processCompressFile(file: File) {
  if (compressLoading || compressWorking || coreFailed) return;

  compressLoading = true;
  updateCompressControls();
  resetCompressOutput(false);
  clearPreview(compressPreview);
  if (compressSource) URL.revokeObjectURL(compressSource.previewUrl);

  const previewUrl = URL.createObjectURL(file);
  compressSource = { file, previewUrl, displayUrl: previewUrl, dimensions: null };
  compressEditor.hidden = false;
  compressSourceName.textContent = file.name;
  compressSourceSize.textContent = formatFileSize(file.size);
  compressPreviewDetail.textContent = "Loading local preview…";
  setCompressStatus(`Reading ${file.name} locally…`, "working");

  try {
    const preview = await showPreview(compressPreview, previewUrl, `Preview of ${file.name}`, file);
    compressSource.dimensions = preview.dimensions;
    compressSource.displayUrl = preview.displayUrl;
    compressPreviewDetail.textContent = preview.dimensions
      ? `Source preview · ${formatDimensions(preview.dimensions)}`
      : "Preview unavailable; the local core will still validate the image bytes.";
    setCompressStatus(
      preview.dimensions
        ? `${file.name} is ready — ${formatDimensions(preview.dimensions)}, ${formatFileSize(file.size)}.`
        : `${file.name} is ready for local validation.`,
      "ready",
    );
  } finally {
    compressLoading = false;
    compressFileInput.value = "";
    updateCompressControls();
  }
}

compressFileInput.addEventListener("change", () => {
  const [file] = compressFileInput.files ?? [];
  if (file) void processCompressFile(file);
});
wireDropZone(compressDropZone, ([file]) => {
  if (file) void processCompressFile(file);
});

compressQuality.addEventListener("input", () => {
  compressQualityValue.value = compressQuality.value;
  if (!compressSource || compressWorking) return;
  resetCompressOutput();
  setCompressStatus("Quality updated. Ready to compress locally.", "ready");
});

compressButton.addEventListener("click", async () => {
  if (!compressSource || compressWorking || coreFailed) return;
  const quality = Number(compressQuality.value);
  if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
    setCompressStatus("Choose an image quality between 1 and 100.", "error");
    return;
  }

  compressWorking = true;
  resetCompressOutput();
  updateCompressControls();
  setCompressStatus(`Compressing ${compressSource.file.name} locally…`, "working");

  try {
    const beforeBytes = compressSource.file.size;
    const compressed = await compressImage(await compressSource.file.arrayBuffer(), quality);
    const format = detectEncodedFormat(compressed);
    if (format !== "jpeg" && format !== "png") {
      throw new Error("The local core returned an unexpected compressed image format.");
    }
    const info = FORMAT_INFO[format];
    const filename = outputFilename(compressSource.file, "compressed", format);
    const afterBytes = compressed.byteLength;
    const savedBytes = Math.max(0, beforeBytes - afterBytes);
    const savedPercent = beforeBytes === 0 ? 0 : (savedBytes / beforeBytes) * 100;
    const previewBlob = new Blob([compressed], { type: info.mime });
    const previewUrl = URL.createObjectURL(previewBlob);
    const preview = await showPreview(
      compressPreview,
      previewUrl,
      `Compressed preview of ${compressSource.file.name}`,
      previewBlob,
    );
    const dimensions = preview.dimensions;

    compressedImage = {
      bytes: compressed,
      filename,
      mime: info.mime,
      previewUrl,
      displayUrl: preview.displayUrl,
      dimensions,
    };
    compressBeforeSize.textContent = formatFileSize(beforeBytes);
    compressAfterSize.textContent = formatFileSize(afterBytes);
    compressSavedPercent.textContent = `${savedPercent.toFixed(1)}% (${formatFileSize(savedBytes)})`;
    compressOutput.hidden = false;
    compressPreviewDetail.textContent = dimensions
      ? `Compressed preview · ${formatDimensions(dimensions)}`
      : "The compressed file is ready, but this browser could not preview it.";
    setCompressStatus(
      savedBytes > 0
        ? `Compressed image ready — saved ${savedPercent.toFixed(1)}%. Location/EXIF metadata was removed.`
        : "No smaller encoding was found, so the file stayed at its original size. Location/EXIF metadata was removed.",
      "success",
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : "The image could not be compressed.";
    setCompressStatus(message, "error");
  } finally {
    compressWorking = false;
    updateCompressControls();
  }
});

compressDownloadButton.addEventListener("click", () => {
  if (compressedImage) {
    downloadImage(compressedImage.bytes, compressedImage.mime, compressedImage.filename);
  }
});

type Tool = "resize" | "convert" | "compress";
const TOOL_TITLES: Record<Tool, string> = {
  resize: "Resize Images — localbench",
  convert: "Convert Images — localbench",
  compress: "Compress Images — localbench",
};
const toolButtons = Array.from(document.querySelectorAll<HTMLButtonElement>("[data-tool]"));
const toolPanels = Array.from(document.querySelectorAll<HTMLElement>("[data-tool-panel]"));
const toolSwitcher = requiredElement<HTMLElement>(".tool-switcher");
const openGraphTitle = requiredElement<HTMLMetaElement>('meta[property="og:title"]');
const twitterTitle = requiredElement<HTMLMetaElement>('meta[name="twitter:title"]');

function isTool(value: string | undefined): value is Tool {
  return value === "resize" || value === "convert" || value === "compress";
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
  const tool = toolFromHash() ?? "resize";
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

setResizeStatus("Choose an image to begin — it never leaves your device.", "ready");
setConvertStatus("Choose an image to begin — it never leaves your device.", "ready");
setCompressStatus("Choose a JPEG or PNG to begin — it never leaves your device.", "ready");
updateResizeControls();
updateConvertControls();
updateCompressControls();
updateJpegNote();

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

window.addEventListener("beforeunload", () => {
  for (const source of [resizeSource, convertSource, compressSource]) {
    if (source) URL.revokeObjectURL(source.previewUrl);
  }
  for (const result of [resizeProcessed, convertProcessed, compressedImage]) {
    if (result) URL.revokeObjectURL(result.previewUrl);
  }
});

if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    void navigator.serviceWorker.register("/sw.js");
  });
}
