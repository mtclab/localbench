/*
 * Live smoke over the keeplocal.tools LANDING site (the Pages project the
 * tool smokes never touch). Plain fetch, no browser - runs from anywhere.
 *
 *   node scripts/site-smoke.mjs                 # live site
 *   BASE=https://preview.example node scripts/site-smoke.mjs
 *
 * The 404 check is the standing gate for the Pages soft-404 class found in
 * the 2026-07-28 estate live-walk: without a 404.html in the deployed output,
 * Cloudflare Pages serves the 200 index for every unknown path, so dead links
 * look alive to crawlers and humans alike.
 */
const BASE = (process.env.BASE ?? "https://keeplocal.tools").replace(/\/$/, "");

let failures = 0;
function check(ok, label, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? ` :: ${detail}` : ""}`);
  if (!ok) failures += 1;
}

async function fetchText(path) {
  const res = await fetch(BASE + path, { redirect: "manual" });
  return { status: res.status, body: await res.text(), headers: res.headers };
}

const home = await fetchText("/");
check(home.status === 200, "home responds 200", `status=${home.status}`);
check(/keeplocal/i.test(home.body), "home is the landing page");
check(/never (upload|leave)/i.test(home.body), "home carries the privacy claim");

const privacy = await fetchText("/privacy");
check(privacy.status === 200, "privacy page responds 200", `status=${privacy.status}`);

const bogus = await fetchText("/site-smoke-expects-404");
check(bogus.status === 404, "unknown path answers 404, not SPA-fallback 200", `status=${bogus.status}`);
check(/not found/i.test(bogus.body), "404 page says so", `bytes=${bogus.body.length}`);
check(/href="\/"/.test(bogus.body), "404 page links back home");

const nosniff = home.headers.get("x-content-type-options");
check(nosniff === "nosniff", "nosniff header served", `got=${nosniff}`);

console.log(failures === 0 ? "\nsite-smoke GREEN" : `\nsite-smoke RED (${failures} failing)`);
process.exitCode = failures === 0 ? 0 : 1;
