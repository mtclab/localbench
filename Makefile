.PHONY: build wasm test site check-local check-notices

APPS = app app-img app-img2pdf app-scrub app-zip

# wasm-pack resolves its --out-dir from the crate directory, so the core lands
# in app/src/wasm; every other app gets the same bytes copied in. They MUST all
# be the same build: an app running a stale core would report notices from a
# contract the interface no longer speaks.
wasm:
	wasm-pack build core-rs --target web --out-dir ../app/src/wasm
	@for app in app-img app-img2pdf app-scrub app-zip; do \
	  mkdir -p $$app/src/wasm && cp app/src/wasm/* $$app/src/wasm/; \
	done

build: wasm
	@for app in $(APPS); do npm --prefix $$app run build || exit 1; done

test:
	cargo test --manifest-path core-rs/Cargo.toml

# The provable-local gate over every built app, plus the gate ON that gate:
# check-local.test.mjs breaks a real dist/ one property at a time and proves the
# checker goes red for each. A green check-local run means nothing without it.
#
# The runtime half of the proof is scripts/receipt-proof.mjs, which drives the
# built dist in a real browser and proves the receipt's request counter sees the
# WORKER's timeline, not just the window's. It runs on the staging box, never
# here:
#   scp -r <app>/dist scripts/receipt-proof.mjs bilvi-dev-stage@10.96.16.18:~/localbench-smoke/
#   ssh bilvi-dev-stage@10.96.16.18 'cd ~/localbench-smoke && \
#     CHROME_PATH=$$HOME/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome \
#     node receipt-proof.mjs dist'
check-local:
	@for app in $(APPS); do \
	  node scripts/check-local.mjs $$app/dist || exit 1; \
	done
	node --test scripts/check-local.test.mjs

# The honesty gate: a notice the Rust core returns must reach the user.
#
# This half runs anywhere — it reads the FileResult contract out of the Rust
# source, checks every worker and every answer against it, and drives the real
# compiled core over the fixtures in Node so a fixture that stopped provoking a
# notice fails loudly instead of passing vacuously.
#
# The other half is scripts/notice-proof.mjs, which proves the sentences reach a
# SCREEN. It needs a browser, so it runs on the staging box, never here:
#   node scripts/notice-fixtures.mjs ./notice-corpus
#   scp -r app*/dist scripts/notice-proof.mjs notice-corpus test-corpus \
#     scrub-corpus img2pdf-corpus bilvi-dev-stage@10.96.16.18:~/localbench-notice/
#   ssh bilvi-dev-stage@10.96.16.18 'cd ~/localbench-notice && \
#     CHROME_PATH=$$HOME/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome \
#     node notice-proof.mjs'
check-notices:
	node --test scripts/notice-plumbing.test.mjs

# Assembles site/dist: the landing page plus the canonical shared/identity.css.
site:
	node scripts/build-site.mjs

