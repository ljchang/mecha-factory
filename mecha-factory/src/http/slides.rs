//! The PowerPoint content add-in's wrapper page: the one buildable native
//! embed (SLIDES-RESEARCH.md §1 route 4, §3), and deliberately not an app —
//! no crate, no process, nothing in mecha-core. PowerPoint's own webview
//! loads this page; the page asks once for a poll's projector URL, keeps it
//! **in the deck** via Office.js settings (the Mentimeter pattern — the
//! document carries an URL, never live content), and swaps a quiet
//! edit-view placeholder for the live screen when the show starts.
//!
//! Fail-soft is the design's one hard rule here: `ActiveViewChanged` is the
//! joint Microsoft has broken before (Mac 16.94 killed it for months), so
//! its absence — or Office.js failing entirely — degrades to "chart live in
//! both views", never to a blank object on a slide mid-lecture.
//!
//! These routes declare their **own** CSP instead of inheriting the gate's:
//! office.js loads from Microsoft's CDN (self-hosting it is unsupported),
//! and the chart is this origin's own screen page in a frame — two
//! allowances the gate's form policy rightly refuses everywhere else. The
//! header middleware only fills in what a handler left unset, so declaring
//! here is the override. `frame-ancestors` is omitted on purpose: the page
//! exists to be embedded by PowerPoint's webview.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;

use super::{v1, Shared};
use crate::config::Origin;

fn addin_csp() -> String {
    [
        "default-src 'none'",
        "style-src 'self'",
        "script-src 'self' https://appsforoffice.microsoft.com",
        "img-src 'self' data:",
        // office.js phones its own infrastructure; the page itself fetches
        // nothing.
        "connect-src 'self' https://*.microsoft.com https://*.office.com",
        "frame-src 'self'",
        "base-uri 'none'",
    ]
    .join("; ")
}

/// `GET /slides/addin` — what the manifest's SourceLocation points at.
pub async fn page(
    State(app): State<Shared>,
    Extension(origin): Extension<Origin>,
) -> Response {
    if let Some(refusal) = v1::not_on_gate(&origin) {
        return refusal;
    }
    let theme = mecha_manifest::Theme::by_name(&app.config.theme);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Live poll</title>\n\
         <style>{css}</style>\n\
         </head>\n<body class=\"addin\">\n{BODY}\n\
         <script src=\"https://appsforoffice.microsoft.com/lib/1/hosted/office.js\"></script>\n\
         <script src=\"/slides/addin.js\" defer></script>\n\
         </body>\n</html>\n",
        css = theme.css(),
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (header::CONTENT_SECURITY_POLICY, addin_csp()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
        ],
        html,
    )
        .into_response()
}

/// `GET /slides/addin.js` — the wrapper's own script, external because even
/// this page keeps `script-src` to named origins rather than inline.
pub async fn script(Extension(origin): Extension<Origin>) -> Response {
    if let Some(refusal) = v1::not_on_gate(&origin) {
        return refusal;
    }
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/javascript; charset=utf-8".to_string(),
        )],
        ADDIN_JS,
    )
        .into_response()
}

const BODY: &str = r#"<main class="booking">
<div id="setup" hidden>
  <h1>Live poll</h1>
  <p>Paste this poll's projector URL — <code>factory-publish polls create</code>
  printed it on the <strong>projector:</strong> line, and the TUI's
  <code>/polls</code> shows it under <strong>s</strong>.</p>
  <p><input id="url" type="url" size="60"
    placeholder="https://…/p/…/…/screen/…"></p>
  <p><button id="save" type="button">Use this poll</button></p>
  <p id="complaint" class="stale" role="alert" hidden></p>
</div>
<div id="standby" hidden>
  <p><strong>Live poll ready</strong> — the chart appears when the slideshow
  starts.</p>
  <p id="which" class="zone"></p>
  <p><button id="preview" type="button">preview now</button>
  <button id="change" type="button">change poll</button></p>
</div>
<iframe id="chart" title="Live poll results" hidden></iframe>
<p id="note" class="zone" hidden></p>
</main>"#;

const ADDIN_JS: &str = r#"// The add-in wrapper. The deck stores a URL, never content; the chart is
// this origin's own screen page in a frame. Anything that fails, fails
// toward showing the live chart — a blank object on a slide mid-lecture is
// the one outcome this file exists to prevent.
(function () {
  "use strict";
  var KEY = "mecha-screen-url";
  var el = function (id) { return document.getElementById(id); };
  var setup = el("setup"), standby = el("standby"), chart = el("chart");
  var note = el("note"), complaint = el("complaint"), which = el("which");
  var settings = null; // Office document settings, when Office arrives

  var stored = function () {
    var fromDoc = settings && settings.get(KEY);
    if (fromDoc) return fromDoc;
    try { return window.localStorage.getItem(KEY); } catch (e) { return null; }
  };
  var store = function (url) {
    try { window.localStorage.setItem(KEY, url); } catch (e) {}
    if (settings) {
      settings.set(KEY, url);
      settings.saveAsync(function () {});
    }
  };
  // Only this origin's own poll pages may fill the frame — the CSP
  // enforces it, this just says so in words instead of a blank frame.
  var acceptable = function (url) {
    try {
      var parsed = new URL(url);
      return parsed.origin === window.location.origin &&
        parsed.pathname.indexOf("/p/") === 0;
    } catch (e) { return false; }
  };
  var show = function (view) {
    setup.hidden = view !== "setup";
    standby.hidden = view !== "standby";
    chart.hidden = view !== "chart";
    if (view === "chart") {
      var url = stored();
      if (chart.getAttribute("src") !== url) chart.setAttribute("src", url);
    }
  };
  var tell = function (text) { note.hidden = false; note.textContent = text; };

  el("save").addEventListener("click", function () {
    var url = el("url").value.trim();
    if (!acceptable(url)) {
      complaint.hidden = false;
      complaint.textContent =
        "That is not this gate's projector URL — it starts with " +
        window.location.origin + "/p/…";
      return;
    }
    store(url);
    which.textContent = url;
    show("standby");
  });
  el("change").addEventListener("click", function () { show("setup"); });
  el("preview").addEventListener("click", function () { show("chart"); });

  // Slideshow shows the chart; edit view stands by. Both the view query
  // and the change event are the historically fragile joints, so every
  // failure path lands on the chart, live in both views.
  var byView = function () {
    var url = stored();
    if (!url) { show("setup"); return; }
    which.textContent = url;
    var live = function () { show("chart"); };
    try {
      Office.context.document.getActiveViewAsync(function (result) {
        if (result.status !== Office.AsyncResultStatus.Succeeded) { live(); return; }
        show(result.value === "read" ? "chart" : "standby");
      });
      Office.context.document.addHandlerAsync(
        Office.EventType.ActiveViewChanged,
        function (args) {
          show(args.activeView === "read" ? "chart" : "standby");
        },
        function (result) {
          if (result.status !== Office.AsyncResultStatus.Succeeded) {
            tell("This PowerPoint doesn't announce the slideshow — showing the chart live in both views.");
            live();
          }
        }
      );
    } catch (e) { live(); }
  };

  if (window.Office && Office.onReady) {
    Office.onReady(function () {
      settings = Office.context && Office.context.document &&
        Office.context.document.settings;
      byView();
    });
    // office.js that never readies (a dead CDN mid-lecture) must not mean
    // a blank object: after a beat, fall back to what this browser knows.
    window.setTimeout(function () {
      if (settings === null && stored()) {
        tell("Office.js didn't answer — showing the chart from this machine's last poll.");
        show("chart");
      }
    }, 4000);
  } else {
    // No Office at all: a plain browser looking at the wrapper. Behave
    // like a small launcher for the screen page.
    if (stored()) { show("chart"); } else { show("setup"); }
  }
})();
"#;
