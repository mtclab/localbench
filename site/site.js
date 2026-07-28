/*
 * keeplocal.tools — landing behaviour.
 *
 * Two jobs: carry the theme choice (same storage key as every tool, so a dark
 * choice made here survives the first click through to pdf.keeplocal.tools),
 * and take the live readings for the receipt.
 *
 * A classic script in <head> so the theme is applied before first paint.
 */
(function () {
  "use strict";

  /* ---------------------------------------------------------------- theme */

  // Shared across keeplocal.tools and every *.keeplocal.tools tool app. The key
  // must stay identical or dark mode is lost on the first click.
  var THEME_KEY = "keeplocal-theme";
  var root = document.documentElement;

  function storedTheme() {
    try {
      var stored = localStorage.getItem(THEME_KEY);
      if (stored === "light" || stored === "dark") return stored;
    } catch (error) {
      /* Storage can be blocked; fall through to the OS preference. */
    }
    return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }

  function applyTheme(theme) {
    root.dataset.theme = theme;
    var meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.content = theme === "dark" ? "#0b120f" : "#f6f7f5";

    var toggle = document.getElementById("theme-toggle");
    var label = document.getElementById("theme-label");
    if (toggle) {
      toggle.setAttribute("aria-pressed", String(theme === "dark"));
      toggle.setAttribute("aria-label", "Use " + (theme === "dark" ? "light" : "dark") + " theme");
    }
    if (label) label.textContent = theme === "dark" ? "Light" : "Dark";
  }

  applyTheme(storedTheme());

  /* -------------------------------------------------------------- receipt */

  function writeProof(name, text, state) {
    var slots = document.querySelectorAll('[data-proof="' + name + '"]');
    for (var i = 0; i < slots.length; i++) {
      slots[i].textContent = text;
      if (state) slots[i].dataset.state = state;
      else delete slots[i].dataset.state;
    }
  }

  function measure() {
    // Every resource this document has actually fetched, checked against this
    // origin. A site that uploads your file cannot print a zero here.
    var external = performance.getEntriesByType("resource").filter(function (entry) {
      try {
        return new URL(entry.name, location.href).origin !== location.origin;
      } catch (error) {
        return true;
      }
    });
    writeProof("external", String(external.length), external.length === 0 ? null : "fail");

    // Read the policy off the document itself rather than printing a claim.
    var cspMeta = document.querySelector('meta[http-equiv="Content-Security-Policy"]');
    var directive = null;
    if (cspMeta) {
      var parts = cspMeta.content.split(";");
      for (var i = 0; i < parts.length; i++) {
        var trimmed = parts[i].trim();
        if (/^connect-src(\s|$)/i.test(trimmed)) {
          directive = trimmed;
          break;
        }
      }
    }
    writeProof("connect-src", directive || "Not declared", directive ? null : "fail");

    var thirdParty = Array.prototype.filter.call(
      document.querySelectorAll("script[src]"),
      function (script) {
        try {
          return new URL(script.src, location.href).origin !== location.origin;
        } catch (error) {
          return true;
        }
      },
    );
    writeProof("scripts", String(thirdParty.length), thirdParty.length === 0 ? null : "fail");

    var cookies = document.cookie ? document.cookie.split(";").length : 0;
    writeProof("cookies", String(cookies), cookies === 0 ? null : "fail");

    writeProof(
      "stamp",
      new Date().toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      }),
    );
  }

  /* ----------------------------------------------------------------- wire */

  function ready() {
    applyTheme(root.dataset.theme === "dark" ? "dark" : "light");

    var toggle = document.getElementById("theme-toggle");
    if (toggle) {
      toggle.addEventListener("click", function () {
        var next = root.dataset.theme === "dark" ? "light" : "dark";
        try {
          localStorage.setItem(THEME_KEY, next);
        } catch (error) {
          /* A blocked store still themes this page for this visit. */
        }
        applyTheme(next);
      });
    }

    var recheck = document.querySelectorAll("[data-proof-recheck]");
    for (var i = 0; i < recheck.length; i++) {
      recheck[i].addEventListener("click", measure);
    }

    measure();
    // Re-read once everything has settled, so the count covers the whole load.
    window.addEventListener("load", measure);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ready);
  } else {
    ready();
  }
})();
