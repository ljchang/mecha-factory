#!/usr/bin/env python3
"""Load a served bundle in a real browser and report what the policy blocked.

    factory-publish serve <bundle> --class compute --port 8347 &
    scripts/csp-probe.py http://127.0.0.1:8347/ --expect-text "mean of arange"

Why this exists: a `compute` bundle loads its Python runtime, its workers and
its WebAssembly at *runtime*. No static check can tell you whether it boots
under `script-src 'self' 'wasm-unsafe-eval'` / `connect-src 'self'` — the
vendoring gate proves nothing off-origin is *referenced*, and the runtime check
proves the two known CDN literals are gone, but only a browser proves the thing
actually runs. Everything a program says about its own work is hearsay; this
grades the artifact.

It reports three things, and the second is the one that matters:

  - **CSP violations**, from `securitypolicyviolation` events, each with the
    directive that fired and the URI it blocked. A notebook that silently shows
    a spinner forever is one of these, and the message says which directive.
  - **Requests that left the origin.** Recorded by URL, because a bundle that
    boots by fetching Pyodide from a CDN has not been vendored — it has been
    served somewhere the policy was not enforced.
  - **Console errors** and whether the expected text ever appeared.

Exit code is 0 only when nothing left the origin, nothing was blocked, and the
expected text rendered. Requires `playwright install firefox`.
"""

from __future__ import annotations

import argparse
import sys
from urllib.parse import urlparse


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url")
    parser.add_argument(
        "--expect-text",
        help="text that must appear once the page has run; without it, only "
        "the policy and network checks are made",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=90.0,
        help="seconds to wait for the expected text (a cold Pyodide boot is slow)",
    )
    parser.add_argument("--screenshot", help="write a PNG here, for a human to look at")
    args = parser.parse_args()

    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print(
            "playwright is not installed: pip install playwright && "
            "playwright install firefox",
            file=sys.stderr,
        )
        return 2

    origin = urlparse(args.url)
    origin_host = f"{origin.scheme}://{origin.netloc}"

    violations: list[dict] = []
    off_origin: list[str] = []
    console_errors: list[str] = []
    page_errors: list[str] = []

    with sync_playwright() as p:
        browser = p.firefox.launch()
        page = browser.new_page()

        # Every request whose URL is not our origin. `data:` and `blob:` are
        # not off-origin — they are content the page already had.
        def on_request(request):
            url = request.url
            if url.startswith(("data:", "blob:", "about:")):
                return
            if not url.startswith(origin_host):
                off_origin.append(url)

        page.on("request", on_request)
        page.on(
            "console",
            lambda m: console_errors.append(m.text) if m.type == "error" else None,
        )
        page.on("pageerror", lambda e: page_errors.append(str(e)))

        # The CSP report has to come from inside the page: Firefox surfaces
        # `securitypolicyviolation` as a DOM event and *not* reliably as a
        # console error, so listening on the console alone reports a page that
        # quietly did nothing.
        page.add_init_script(
            """
            window.__cspViolations = [];
            document.addEventListener('securitypolicyviolation', (e) => {
              window.__cspViolations.push({
                directive: e.effectiveDirective || e.violatedDirective,
                blocked: e.blockedURI,
                source: e.sourceFile || '',
                line: e.lineNumber || 0,
              });
            });
            """
        )

        page.goto(args.url, wait_until="domcontentloaded")

        found = True
        if args.expect_text:
            try:
                page.wait_for_function(
                    "text => document.body && document.body.innerText.includes(text)",
                    arg=args.expect_text,
                    timeout=args.timeout * 1000,
                )
            except Exception:
                found = False

        violations = page.evaluate("window.__cspViolations || []")
        if args.screenshot:
            page.screenshot(path=args.screenshot, full_page=True)
        browser.close()

    print(f"url                {args.url}")
    print(f"csp violations     {len(violations)}")
    for v in violations[:20]:
        where = f" at {v['source']}:{v['line']}" if v.get("source") else ""
        print(f"  {v['directive']}  blocked {v['blocked']}{where}")
    print(f"off-origin loads   {len(off_origin)}")
    for url in sorted(set(off_origin))[:20]:
        print(f"  {url}")
    print(f"console errors     {len(console_errors)}")
    for message in console_errors[:10]:
        print(f"  {message[:200]}")
    print(f"page errors        {len(page_errors)}")
    for message in page_errors[:10]:
        print(f"  {message[:200]}")
    if args.expect_text:
        print(f"expected text      {'found' if found else 'NOT FOUND'}")

    ok = not violations and not off_origin and (found if args.expect_text else True)
    print(f"\n{'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
