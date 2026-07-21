// Targeted check: OCR an accented-Latin image into a searchable PDF and confirm
// (post-pull, via pdftotext) that accented text survives in the text layer.
import { chromium } from "playwright";
import { readFile } from "node:fs/promises";
import path from "node:path";

const BASE = process.env.BASE ?? "http://127.0.0.1:5062";
const CORPUS = process.env.CORPUS ?? "./ocr-samples";
const OUT = process.env.OUT ?? "./shots-ocr";
const opts = { args: ["--no-sandbox"] };
if (process.env.CHROME_PATH) opts.executablePath = process.env.CHROME_PATH;

const browser = await chromium.launch(opts);
const page = await browser.newPage();
await page.goto(BASE, { waitUntil: "networkidle" });
await page.waitForFunction(() => /\d+\.\d+\.\d+/.test(document.querySelector("#core-version")?.textContent ?? ""), { timeout: 20000 });
const img = await readFile(path.join(CORPUS, "accented.png"));
await page.setInputFiles("#ocr-file-input", { name: "accented.png", mimeType: "image/png", buffer: img });
await page.waitForSelector("#ocr-editor:not([hidden])", { timeout: 10000 });
await page.check("#output-mode-pdf").catch(async () => { await page.click("#output-mode-pdf"); });
await page.click("#extract-button");
await page.waitForFunction(() => { const b = document.querySelector("#pdf-download-button"); return b && !b.disabled; }, { timeout: 45000 });
const dl = await Promise.all([
  page.waitForEvent("download", { timeout: 15000 }),
  page.click("#pdf-download-button"),
]).then(([d]) => d);
const out = path.join(OUT, "accented-searchable.pdf");
await dl.saveAs(out);
console.log(`SAVED ${out}`);
await browser.close();
