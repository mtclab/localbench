import "./style.css";

// Protocol mirrors app/src/core.worker.ts. The worker posts "ready" (with the
// Rust core version) once on load; page-count requests are id-matched.
type WorkerRequest = { id: number; type: "page-count"; bytes: ArrayBuffer };

type WorkerResponse =
  | { type: "ready"; version: string }
  | { type: "result"; id: number; pages: number }
  | { type: "error"; id?: number; message: string };

const worker = new Worker(new URL("./core.worker.ts", import.meta.url), { type: "module" });
const pending = new Map<
  number,
  { resolve: (pages: number) => void; reject: (reason: Error) => void }
>();
let nextRequestId = 1;

worker.addEventListener("message", (event: MessageEvent<WorkerResponse>) => {
  const response = event.data;

  if (response.type === "ready") {
    onCoreReady(response.version);
    return;
  }

  if (response.type === "result") {
    pending.get(response.id)?.resolve(response.pages);
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

function countPages(bytes: ArrayBuffer): Promise<number> {
  const id = nextRequestId++;
  return new Promise<number>((resolve, reject) => {
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, type: "page-count", bytes } satisfies WorkerRequest, [bytes]);
  });
}

const resultEl = document.querySelector<HTMLDivElement>("#result");
const resultTextEl = document.querySelector<HTMLSpanElement>("#result-text");
const fileInputEl = document.querySelector<HTMLInputElement>("#file-input");
const dropZoneEl = document.querySelector<HTMLDivElement>("#drop-zone");
const versionEl = document.querySelector<HTMLElement>("#core-version");

if (!resultEl || !resultTextEl || !fileInputEl || !dropZoneEl || !versionEl) {
  throw new Error("Required interface elements are missing.");
}

// Capture into non-null consts so closures below keep the narrowed type.
const result = resultEl;
const resultText = resultTextEl;
const fileInput = fileInputEl;
const dropZone = dropZoneEl;
const version = versionEl;

function setStatus(text: string, state: "ready" | "working" | "success" | "error") {
  resultText.textContent = text;
  result.dataset.state = state;
}

function onCoreReady(coreVersion: string) {
  version.textContent = `v${coreVersion}`;
}

function onCoreFailed() {
  version.textContent = "Unavailable";
  setStatus("The local processing core could not load.", "error");
}

async function processFile(file: File) {
  const looksLikePdf = file.type === "application/pdf" || file.name.toLowerCase().endsWith(".pdf");
  if (!looksLikePdf) {
    setStatus("Choose a PDF file to continue.", "error");
    return;
  }

  setStatus(`Reading ${file.name} locally…`, "working");
  try {
    // Reading and transferring are orchestration only; PDF byte interpretation stays in Rust.
    const bytes = await file.arrayBuffer();
    const count = await countPages(bytes);
    setStatus(`${count} ${count === 1 ? "page" : "pages"}`, "success");
  } catch (error) {
    const message = error instanceof Error ? error.message : "This PDF could not be read.";
    setStatus(message, "error");
  } finally {
    fileInput.value = "";
  }
}

fileInput.addEventListener("change", () => {
  const [file] = fileInput.files ?? [];
  if (file) void processFile(file);
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
  const [file] = event.dataTransfer?.files ?? [];
  if (file) void processFile(file);
});

// The worker announces readiness (and the core version) on its own via the
// "ready" message, handled in onCoreReady above. No explicit request needed.
setStatus("Drop a PDF to count its pages — it never leaves your device.", "ready");

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

