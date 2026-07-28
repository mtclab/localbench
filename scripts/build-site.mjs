/*
 * Assembles the keeplocal.tools landing page into site/dist for direct upload.
 *
 * The landing has no bundler, but it must not carry its own copy of the design
 * system — that is exactly the duplication this repo just removed from the six
 * tool apps. So the one canonical shared/identity.css is COPIED in at build
 * time and the page links it as /identity.css.
 *
 *   node scripts/build-site.mjs
 *   npx wrangler pages deploy site/dist --project-name=keeplocal-site
 */
import { cp, mkdir, rm, readdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const siteDir = path.join(repoRoot, "site");
const distDir = path.join(siteDir, "dist");
const identityCss = path.join(repoRoot, "shared", "identity.css");

// Everything in site/ except the build output itself.
const sources = ["index.html", "privacy.html", "site.css", "site.js", "icon.svg", "_headers"];

await rm(distDir, { recursive: true, force: true });
await mkdir(distDir, { recursive: true });

for (const name of sources) {
  await cp(path.join(siteDir, name), path.join(distDir, name));
}

// The single source of truth for tokens, type and the receipt component.
await cp(identityCss, path.join(distDir, "identity.css"));

const written = (await readdir(distDir)).sort();
console.log(`Built site/dist with ${written.length} files: ${written.join(", ")}`);
