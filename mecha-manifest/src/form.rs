//! The manifest as an HTML form.
//!
//! Plain HTML and vanilla JavaScript, and for intake that is the simpler option
//! rather than a compromise: there are five forms here, not an application, and
//! a form has no reactive state to manage. Reactivity is a property of the
//! content class — where a page is a *view over data the reader manipulates*
//! there is a case for a framework, and a bounded set of fields submitted once
//! is not that.
//!
//! Two layers, and the first does most of the work:
//!
//! - **HTML5 constraint attributes, emitted from the manifest.** `required`,
//!   `type="email"`, `type="date"`, `min`, `max`, `maxlength`, `pattern`, and a
//!   `<select>` for every enum. The browser validates natively, announces
//!   errors to a screen reader natively, and **needs no JavaScript at all**.
//! - **A small declarative-condition evaluator** for what HTML5 cannot express:
//!   show and hide. A few dozen lines reading the same `show_when` the server
//!   reads, emitted inline as JSON so there are not two copies of the rules.
//!
//! **The server re-evaluates every rule on submit**, because a client-side
//! check is a convenience and never a control. Which is also why the generated
//! JavaScript is allowed to be small and unclever: nothing rests on it.
//!
//! With JavaScript off, every conditional field is simply *shown*. That is the
//! safe direction — a visible optional field is a question someone can ignore,
//! where a hidden required one is a form that cannot be submitted and does not
//! say why.
//!
//! One CSP note, because it is the reason for a decision that otherwise looks
//! fussy: the script and the styles are emitted as external file *references*
//! rather than inline, so the strictest policy (`script-src 'self'`, no
//! `unsafe-inline`) holds with nothing to relax. [`FormPage::assets`] returns
//! the files to write beside the HTML.

use crate::{escape_text, Acknowledgment, Condition, Field, FieldKind, RequestType, Step};

/// How to render. Everything here is presentation; none of it changes what
/// validates.
#[derive(Debug, Clone)]
pub struct FormOptions {
    /// Where the form POSTs.
    pub action: String,
    /// A hidden field carrying the capability token, when the flow has one.
    /// Drafts key off this rather than off `localStorage`, so a resumed
    /// submission survives a different browser.
    pub token: Option<String>,
    /// Values to pre-fill — a resumed draft, or a rejected submission being
    /// shown back with its errors.
    pub values: serde_json::Map<String, serde_json::Value>,
    /// Errors to show beside their fields.
    pub errors: Vec<crate::ValidationError>,
    /// Render only this step. `None` renders every step on one page, which is
    /// what a single-step form wants.
    pub step: Option<String>,
    /// Where `form.css` and `form.js` live, as a URL prefix ending in `/`.
    ///
    /// Supplied by whatever is serving the form, because the URL scheme is the
    /// server's business and this crate's job is the document. It used to be
    /// omitted, which made the links **relative to the form's own URL** — and
    /// from `/f/<handle>/<type>` a bare `form.css` resolves to
    /// `/f/<handle>/form.css`, a 404 served as `text/html` that `nosniff`
    /// then refused. Every form rendered unstyled, in a browser, for as long
    /// as forms have existed; `curl` never noticed because `curl` does not
    /// fetch stylesheets.
    pub assets: String,
    /// The palette. Structure is fixed and shared; a theme supplies token
    /// values and adds no rules, so this changes how a form looks and never
    /// how it behaves. See [`crate::theme`].
    pub theme: crate::Theme,
    /// Whether file fields render as real inputs.
    ///
    /// `false` — the public submission form — renders a file field as a note
    /// saying the upload comes after email verification, and the `<form>`
    /// stays urlencoded. `true` is the post-verification upload page, the one
    /// place a file input exists, and the one place `enctype` is emitted.
    pub file_inputs: bool,
}

impl Default for FormOptions {
    fn default() -> Self {
        FormOptions {
            action: "".into(),
            token: None,
            values: serde_json::Map::new(),
            errors: Vec::new(),
            step: None,
            assets: String::new(),
            theme: crate::theme::NOCTURNE,
            file_inputs: false,
        }
    }
}

/// A rendered form: the page, plus the files it references.
pub struct FormPage {
    pub html: String,
    pub script: &'static str,
    /// Owned rather than `&'static`, because the theme's tokens are prepended:
    /// the stylesheet is now a function of which palette was asked for.
    pub style: String,
}

impl FormPage {
    /// Filename → contents, for whoever is writing the bundle out.
    pub fn assets(&self) -> [(&'static str, &str); 2] {
        [("form.js", self.script), ("form.css", &self.style)]
    }
}

impl RequestType {
    /// Render this request type as a form.
    pub fn form(&self, options: &FormOptions) -> FormPage {
        let mut body = String::new();
        body.push_str(&format!("<h1>{}</h1>\n", escape_text(&self.title)));
        if let Some(description) = &self.description {
            body.push_str(&format!(
                "<p class=\"intro\">{}</p>\n",
                escape_text(description)
            ));
        }

        if !options.errors.is_empty() {
            // A summary at the top *and* a message beside each field: the
            // summary is what a screen reader announces on load, and the
            // per-field message is what a sighted reader needs while fixing it.
            body.push_str(
                "<div class=\"errors\" role=\"alert\"><p>Some answers need attention:</p><ul>\n",
            );
            for error in &options.errors {
                let label = self
                    .field(&error.field)
                    .map(|f| f.label.as_str())
                    .or_else(|| {
                        self.acknowledgments
                            .iter()
                            .find(|a| a.id == error.field)
                            .map(|a| a.label.as_str())
                    })
                    .unwrap_or(error.field.as_str());
                body.push_str(&format!(
                    "<li><a href=\"#{}\">{}</a>: {}</li>\n",
                    escape_text(&error.field),
                    escape_text(label),
                    escape_text(&error.message)
                ));
            }
            body.push_str("</ul></div>\n");
        }

        // No `novalidate`, and deliberately not `novalidate="false"` either —
        // it is a boolean attribute, so *any* value means present, and writing
        // it out to say "please do validate" turns the entire HTML5 constraint
        // layer off. Found by opening the page rather than by reading the code.
        //
        // `enctype` only on the upload page: the public submission form has no
        // file inputs, so emitting it there would change every existing form's
        // bytes to say something false about what it carries.
        let enctype = if options.file_inputs && self.has_file_fields() {
            " enctype=\"multipart/form-data\""
        } else {
            ""
        };
        body.push_str(&format!(
            "<form method=\"post\" action=\"{}\"{enctype}>\n",
            escape_text(&options.action)
        ));
        if let Some(token) = &options.token {
            body.push_str(&format!(
                "<input type=\"hidden\" name=\"_token\" value=\"{}\">\n",
                escape_text(token)
            ));
        }

        if self.steps.is_empty() {
            for field in &self.fields {
                body.push_str(&self.render_field(field, options));
            }
        } else {
            for step in &self.steps {
                if options.step.as_ref().is_some_and(|only| only != &step.id) {
                    continue;
                }
                body.push_str(&self.render_step(step, options));
            }
        }

        for ack in &self.acknowledgments {
            body.push_str(&render_acknowledgment(ack, options));
        }

        body.push_str("<button type=\"submit\">Submit</button>\n</form>\n");

        // The conditions, as data, read by both ends. Emitted as a JSON
        // <script type="application/json"> rather than as executable code, so
        // nothing here needs `script-src 'unsafe-inline'`.
        body.push_str(&format!(
            "<script type=\"application/json\" id=\"conditions\">{}</script>\n",
            // JSON inside a <script> element ends at the first `</script`
            // sequence, whatever the quoting — the one escape a JSON encoder
            // does not do for you.
            serde_json::to_string(&self.condition_map())
                .unwrap_or_else(|_| "{}".into())
                .replace("</", "<\\/")
        ));
        body.push_str(&format!(
            "<script src=\"{}form.js\" defer></script>\n",
            escape_text(&options.assets)
        ));

        let html = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n\
             <meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>{}</title>\n\
             {}\n\
             <link rel=\"stylesheet\" href=\"{}form.css\">\n\
             </head>\n<body>\n{}<main>\n{}</main>\n</body>\n</html>\n",
            escape_text(&self.title),
            // A `data:` URI rather than a `favicon.svg` beside the stylesheet:
            // the stylesheet is a same-origin asset the server routes, and a
            // form may be rendered to a file with no server behind it at all.
            crate::brand::FAVICON_LINK,
            escape_text(&options.assets),
            site_header(),
            body
        );

        FormPage {
            html,
            script: FORM_JS,
            // Tokens first, then the structure that reads them. One file, so a
            // form is one stylesheet request and a theme cannot half-apply.
            style: format!("{}{}", options.theme.css(), FORM_STRUCTURE),
        }
    }

    /// The post-verification upload page: the file fields and nothing else.
    ///
    /// A whole document like [`RequestType::form`], but rendering only the
    /// file fields *visible under the submitted values* (`options.values` —
    /// a conditional file field whose condition failed at submit time must
    /// not be asked for now), with `file_inputs` forced on and no condition
    /// script: every visibility question was already answered by values that
    /// are no longer editable. Submitting with nothing chosen is how an
    /// optional attachment is declined — absence is the decline, so there is
    /// no separate skip verb to get out of sync with the validator.
    pub fn upload_form(&self, options: &FormOptions) -> FormPage {
        let options = FormOptions {
            file_inputs: true,
            ..options.clone()
        };
        let visible: Vec<&Field> = self
            .visible_fields(&options.values)
            .into_iter()
            .filter(|f| matches!(f.kind, FieldKind::File { .. }))
            .collect();
        let any_required = visible.iter().any(|f| f.required);

        let mut body = String::new();
        body.push_str(&format!(
            "<h1>{}</h1>\n<p class=\"intro\">Your email address is verified. {}</p>\n",
            escape_text(&self.title),
            if any_required {
                "One more step: attach what the request asks for."
            } else {
                "If you have files to attach, add them here — or finish without."
            }
        ));

        if !options.errors.is_empty() {
            body.push_str(
                "<div class=\"errors\" role=\"alert\"><p>Some answers need attention:</p><ul>\n",
            );
            for error in &options.errors {
                let label = self
                    .field(&error.field)
                    .map(|f| f.label.as_str())
                    .unwrap_or(error.field.as_str());
                body.push_str(&format!(
                    "<li><a href=\"#{}\">{}</a>: {}</li>\n",
                    escape_text(&error.field),
                    escape_text(label),
                    escape_text(&error.message)
                ));
            }
            body.push_str("</ul></div>\n");
        }

        body.push_str(&format!(
            "<form method=\"post\" action=\"{}\" enctype=\"multipart/form-data\">\n",
            escape_text(&options.action)
        ));
        for field in &visible {
            body.push_str(&self.render_field(field, &options));
        }
        body.push_str(&format!(
            "<button type=\"submit\">{}</button>\n</form>\n",
            if any_required { "Upload and finish" } else { "Finish" }
        ));

        let html = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n\
             <meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>{}</title>\n\
             {}\n\
             <link rel=\"stylesheet\" href=\"{}form.css\">\n\
             </head>\n<body>\n{}<main>\n{}</main>\n</body>\n</html>\n",
            escape_text(&self.title),
            crate::brand::FAVICON_LINK,
            escape_text(&options.assets),
            site_header(),
            body
        );
        FormPage {
            html,
            script: FORM_JS,
            style: format!("{}{}", options.theme.css(), FORM_STRUCTURE),
        }
    }

    /// Every `show_when` in the type, keyed by what it governs. One source of
    /// rules for the browser and the server.
    fn condition_map(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        let mut add = |key: String, condition: &Condition| {
            if let Ok(value) = serde_json::to_value(condition) {
                map.insert(key, value);
            }
        };
        for field in &self.fields {
            if let Some(condition) = &field.show_when {
                add(format!("field:{}", field.name), condition);
            }
        }
        for step in &self.steps {
            if let Some(condition) = &step.show_when {
                add(format!("step:{}", step.id), condition);
            }
        }
        map
    }

    fn render_step(&self, step: &Step, options: &FormOptions) -> String {
        let mut out = format!(
            "<fieldset id=\"step-{}\" data-step=\"{}\"><legend>{}</legend>\n",
            escape_text(&step.id),
            escape_text(&step.id),
            escape_text(&step.title)
        );
        if let Some(description) = &step.description {
            out.push_str(&format!("<p>{}</p>\n", escape_text(description)));
        }
        for name in &step.fields {
            if let Some(field) = self.field(name) {
                out.push_str(&self.render_field(field, options));
            }
        }
        out.push_str("</fieldset>\n");
        out
    }

    fn render_field(&self, field: &Field, options: &FormOptions) -> String {
        let name = escape_text(&field.name);
        let error = options.errors.iter().find(|e| e.field == field.name);
        let described_by = {
            let mut ids = Vec::new();
            if field.help.is_some() {
                ids.push(format!("{name}-help"));
            }
            if error.is_some() {
                ids.push(format!("{name}-error"));
            }
            if ids.is_empty() {
                String::new()
            } else {
                format!(" aria-describedby=\"{}\"", ids.join(" "))
            }
        };
        let invalid = if error.is_some() {
            " aria-invalid=\"true\""
        } else {
            ""
        };
        let required = if field.required { " required" } else { "" };
        let value = options.values.get(&field.name);

        let control = match &field.kind {
            FieldKind::LongText { max_length } => format!(
                "<textarea id=\"{name}\" name=\"{name}\" maxlength=\"{max_length}\" \
                 rows=\"6\"{required}{invalid}{described_by}>{}</textarea>",
                value
                    .and_then(|v| v.as_str())
                    .map(escape_text)
                    .unwrap_or_default()
            ),
            FieldKind::Select { options: choices } => {
                let mut out = format!(
                    "<select id=\"{name}\" name=\"{name}\"{required}{invalid}{described_by}>\n"
                );
                // An explicit empty option, so a required select starts unchosen
                // rather than silently defaulting to whatever is first — a
                // default answer to a question nobody read is worse than a
                // blank one.
                out.push_str("<option value=\"\">Choose…</option>\n");
                for choice in choices {
                    let selected = if value.and_then(|v| v.as_str()) == Some(choice.value.as_str())
                    {
                        " selected"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "<option value=\"{}\"{selected}>{}</option>\n",
                        escape_text(&choice.value),
                        escape_text(&choice.label)
                    ));
                }
                out.push_str("</select>");
                out
            }
            FieldKind::MultiSelect {
                options: choices, ..
            } => {
                let chosen: Vec<&str> = value
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let mut out = String::from("<div class=\"choices\">\n");
                for choice in choices {
                    let checked = if chosen.contains(&choice.value.as_str()) {
                        " checked"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "<label class=\"choice\"><input type=\"checkbox\" name=\"{name}\" \
                         value=\"{}\"{checked}> {}</label>\n",
                        escape_text(&choice.value),
                        escape_text(&choice.label)
                    ));
                }
                out.push_str("</div>");
                out
            }
            FieldKind::Bool => {
                let checked = if value == Some(&serde_json::Value::Bool(true)) {
                    " checked"
                } else {
                    ""
                };
                format!(
                    "<input type=\"checkbox\" id=\"{name}\" name=\"{name}\" \
                     value=\"true\"{checked}{invalid}{described_by}>"
                )
            }
            // Its own arm on purpose: the catch-all below ends in
            // `unreachable!`, and a kind that is not a scalar input must never
            // be allowed to fall toward it.
            FieldKind::File { max_bytes, accept } => {
                let kinds: Vec<&str> = accept.iter().map(|t| t.extension()).collect();
                let megabytes = max_bytes.div_ceil(1024 * 1024);
                if options.file_inputs {
                    let tokens: Vec<&str> = accept
                        .iter()
                        .flat_map(|t| t.accept_tokens().iter().copied())
                        .collect();
                    format!(
                        "<input type=\"file\" id=\"{name}\" name=\"{name}\" \
                         accept=\"{}\"{required}{invalid}{described_by}>\n\
                         <p class=\"file-limits\">{} — up to {megabytes} MB</p>",
                        escape_text(&tokens.join(",")),
                        escape_text(&kinds.join(", "))
                    )
                } else {
                    // The submission form carries no file input at all — a
                    // note in the control's place, so the requester knows the
                    // ask is coming without being offered a control that
                    // would not be accepted. (`label for` pointing at a <p>
                    // is inert, which is fine: there is nothing to focus.)
                    format!(
                        "<p class=\"file-note\" id=\"{name}\">You'll be asked to attach \
                         this ({}, up to {megabytes} MB) after verifying your email \
                         address.</p>",
                        escape_text(&kinds.join(", "))
                    )
                }
            }
            other => {
                let (input_type, attributes) = match other {
                    FieldKind::Text {
                        max_length,
                        pattern,
                    } => (
                        "text",
                        format!(
                            " maxlength=\"{max_length}\"{}",
                            pattern
                                .as_ref()
                                .map(|p| format!(" pattern=\"{}\"", escape_text(p)))
                                .unwrap_or_default()
                        ),
                    ),
                    FieldKind::Email { max_length } => {
                        ("email", format!(" maxlength=\"{max_length}\""))
                    }
                    FieldKind::Url { max_length } => {
                        ("url", format!(" maxlength=\"{max_length}\""))
                    }
                    FieldKind::Date { min, max } => (
                        "date",
                        format!(
                            "{}{}",
                            min.as_ref()
                                .map(|m| format!(" min=\"{}\"", escape_text(m)))
                                .unwrap_or_default(),
                            max.as_ref()
                                .map(|m| format!(" max=\"{}\"", escape_text(m)))
                                .unwrap_or_default()
                        ),
                    ),
                    FieldKind::Integer { min, max } => (
                        "number",
                        format!(
                            " step=\"1\"{}{}",
                            min.map(|m| format!(" min=\"{m}\"")).unwrap_or_default(),
                            max.map(|m| format!(" max=\"{m}\"")).unwrap_or_default()
                        ),
                    ),
                    _ => unreachable!("the other kinds are handled above"),
                };
                format!(
                    "<input type=\"{input_type}\" id=\"{name}\" name=\"{name}\"{attributes} \
                     value=\"{}\"{required}{invalid}{described_by}>",
                    value
                        .map(|v| match v {
                            serde_json::Value::String(s) => escape_text(s),
                            other => escape_text(&other.to_string()),
                        })
                        .unwrap_or_default()
                )
            }
        };

        let mut out = format!(
            "<div class=\"field\" data-field=\"{name}\"{}>\n<label for=\"{name}\">{}{}</label>\n",
            // With JavaScript off nothing hides anything, so a conditional
            // field is simply shown — the safe direction. The attribute is what
            // the evaluator keys on.
            if field.show_when.is_some() {
                " data-conditional=\"true\""
            } else {
                ""
            },
            escape_text(&field.label),
            if field.required {
                " <span class=\"req\" aria-hidden=\"true\">*</span>"
            } else {
                ""
            }
        );
        if let Some(help) = &field.help {
            out.push_str(&format!(
                "<p class=\"help\" id=\"{name}-help\">{}</p>\n",
                escape_text(help)
            ));
        }
        out.push_str(&control);
        out.push('\n');
        if let Some(error) = error {
            out.push_str(&format!(
                "<p class=\"error\" id=\"{name}-error\">{}</p>\n",
                escape_text(&error.message)
            ));
        }
        out.push_str("</div>\n");
        out
    }
}

/// The site header a rendered form carries: the mark, linked to the gate
/// root. Public chrome only — a form is a stranger's page, and account
/// affordances belong to the gate's own pages, not here.
pub fn site_header() -> String {
    format!(
        "<header class=\"site\"><a class=\"mark\" href=\"/\" aria-label=\"mecha\">{}</a></header>\n",
        crate::brand::LOGO_MONO_SVG
    )
}

fn render_acknowledgment(ack: &Acknowledgment, options: &FormOptions) -> String {
    let id = escape_text(&ack.id);
    let checked = if options.values.get(&ack.id) == Some(&serde_json::Value::Bool(true)) {
        " checked"
    } else {
        ""
    };
    let error = options.errors.iter().find(|e| e.field == ack.id);
    let mut out = format!("<div class=\"field ack\" data-field=\"{id}\">\n");
    out.push_str(&format!(
        "<label for=\"{id}\"><input type=\"checkbox\" id=\"{id}\" name=\"{id}\" \
         value=\"true\" required{checked}{}> {}</label>\n",
        if error.is_some() {
            format!(" aria-invalid=\"true\" aria-describedby=\"{id}-error\"")
        } else {
            String::new()
        },
        escape_text(&ack.label)
    ));
    if let Some(description) = &ack.description {
        out.push_str(&format!(
            "<p class=\"help\">{}</p>\n",
            escape_text(description)
        ));
    }
    if let Some(link) = &ack.info_link {
        // `rel="noopener noreferrer"` because the destination is named in our
        // own manifest but opens in the reader's browser, and `target="_blank"`
        // without it hands the opener a handle back.
        out.push_str(&format!(
            "<p class=\"help\"><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">\
             What this means</a></p>\n",
            escape_text(link)
        ));
    }
    if let Some(error) = error {
        out.push_str(&format!(
            "<p class=\"error\" id=\"{id}-error\">{}</p>\n",
            escape_text(&error.message)
        ));
    }
    out.push_str("</div>\n");
    out
}

/// The declarative-condition evaluator.
///
/// It must agree with `Condition::holds` exactly, **including the cases that
/// look like mistakes** — `ne` against an unsubmitted field not holding, a
/// blank string counting as absent, every malformed comparison answering false.
/// The server re-evaluates everything, so a disagreement is not a hole; it is
/// a form that hides a field the server then demands, which is worse to debug.
const FORM_JS: &str = r#"// Generated by mecha-manifest. Mirrors Condition::holds in condition.rs —
// keep the two in step, including the cases that look like mistakes.
(function () {
  var node = document.getElementById('conditions');
  if (!node) return;
  var rules = JSON.parse(node.textContent || '{}');
  var form = document.querySelector('form');
  if (!form) return;

  function raw(name) {
    var els = form.querySelectorAll('[name="' + CSS.escape(name) + '"]');
    if (!els.length) return undefined;
    var first = els[0];
    if (first.type === 'checkbox') {
      if (els.length > 1) {
        var picked = [];
        els.forEach(function (el) { if (el.checked) picked.push(el.value); });
        return picked;
      }
      return first.checked;
    }
    return first.value;
  }

  function present(v) {
    if (v === undefined || v === null) return false;
    if (typeof v === 'string') return v.trim() !== '';
    if (Array.isArray(v)) return v.length > 0;
    return true;
  }

  function num(v) {
    if (typeof v === 'number') return v;
    if (typeof v === 'string' && v.trim() !== '') {
      var n = Number(v.trim());
      return isNaN(n) ? null : n;
    }
    return null;
  }

  function holds(rule) {
    var a = raw(rule.field);
    var b = rule.value;
    switch (rule.op) {
      case 'present': return present(a);
      case 'absent': return !present(a);
      case 'is_true': return a === true;
      case 'is_false': return a === false;
      case 'eq': return a !== undefined && a === b;
      case 'ne': return a !== undefined && a !== b;
      case 'in': return a !== undefined && Array.isArray(b) && b.indexOf(a) !== -1;
      case 'not_in': return a !== undefined && Array.isArray(b) && b.indexOf(a) === -1;
      case 'gt': case 'gte': case 'lt': case 'lte': {
        var x = num(a), y = num(b);
        if (x === null || y === null) return false;
        if (rule.op === 'gt') return x > y;
        if (rule.op === 'gte') return x >= y;
        if (rule.op === 'lt') return x < y;
        return x <= y;
      }
      default: return false;
    }
  }

  // A hidden control must not submit a value, or the server rejects the
  // record as carrying a field that was never shown. `disabled` is what
  // stops a browser sending it; `hidden` alone does not.
  function apply() {
    Object.keys(rules).forEach(function (key) {
      var parts = key.split(':');
      var selector = parts[0] === 'step'
        ? '[data-step="' + CSS.escape(parts[1]) + '"]'
        : '[data-field="' + CSS.escape(parts[1]) + '"]';
      var el = form.querySelector(selector);
      if (!el) return;
      var show = holds(rules[key]);
      el.hidden = !show;
      el.querySelectorAll('input, select, textarea').forEach(function (control) {
        control.disabled = !show;
      });
    });
  }

  form.addEventListener('input', apply);
  form.addEventListener('change', apply);
  apply();
})();
"#;

const FORM_STRUCTURE: &str = r#"
/* mecha-manifest — the structural sheet. One column, one layout, every colour
   a token. Nothing here is a hue: a theme supplies the values and adds no
   rules, which is what makes changing the look a swap rather than a rewrite.

   No @import and no hosted font. An imported stylesheet is an external
   reference — the gate blocks it under `style-src 'self'`, and the publish
   gate fails a bundle for one. Vendoring a woff2 and serving it from our own
   origin is the supported route. */

* { box-sizing: border-box; }

body {
  margin: 0;
  background: var(--ground);
  color: var(--text);
  font-family: var(--font-sans);
  font-size: 16px;
  line-height: 1.55;
  -webkit-font-smoothing: antialiased;
}

main {
  /* A readable measure, not a mobile one. Forms narrow themselves below;
     pages that carry tables widen themselves further down. */
  max-width: 46rem;
  margin: 0 auto;
  padding: 3rem 1.5rem 6rem;
}

/* A form is read one line at a time, and a long measure makes a column of
   inputs feel like a table — so the form element keeps the old narrow
   measure inside whatever page it is on. */
main > form { max-width: 34rem; }

/* The account page's ledgers are tables, and tables want room. `:has` is
   the page classifying itself: no markup change, no second stylesheet. */
main:has(table) { max-width: 72rem; }

/* Commands a person copies — the pairing instruction — wrap instead of
   walking off the edge of the screen. */
pre {
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 0.75rem 1rem;
  font-family: var(--font-mono);
  font-size: 0.875rem;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

h1 {
  font-size: 1.75rem;
  font-weight: 500;
  letter-spacing: -0.02em;
  line-height: 1.2;
  margin: 0 0 0.75rem;
}

.intro {
  color: var(--muted);
  margin: 0 0 2.5rem;
  font-size: 1.0625rem;
}

/* --- fields ------------------------------------------------------------- */

.field { margin: 0 0 1.75rem; }
.field[hidden] { display: none; }

label {
  display: block;
  /* Mono, deliberately. A field label names a value rather than describing
     one, which is nearer to code than to prose — and it is the single choice
     that stops a generated form looking generic. */
  font-family: var(--font-mono);
  font-size: 0.8125rem;
  font-weight: 500;
  letter-spacing: 0.01em;
  margin: 0 0 0.5rem;
}

.req {
  color: var(--muted);
  font-weight: 400;
}

.help {
  color: var(--muted);
  font-size: 0.875rem;
  margin: -0.25rem 0 0.5rem;
}

input[type=text], input[type=email], input[type=url], input[type=date],
input[type=number], input[type=tel], select, textarea {
  display: block;
  width: 100%;
  padding: 0.625rem 0.75rem;
  font: inherit;
  font-size: 0.9375rem;
  color: var(--text);
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  transition: border-color .12s ease, box-shadow .12s ease;
}

textarea { min-height: 7rem; resize: vertical; line-height: 1.6; }

input::placeholder, textarea::placeholder { color: var(--muted); opacity: .7; }

input[type=file] {
  display: block;
  width: 100%;
  padding: 0.625rem 0.75rem;
  font: inherit;
  font-size: 0.875rem;
  color: var(--text);
  background: var(--surface);
  border: 1px dashed var(--line);
  border-radius: var(--radius);
}

input[type=file]::file-selector-button {
  font: inherit;
  font-size: 0.875rem;
  color: var(--text);
  background: var(--ground);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 0.375rem 0.75rem;
  margin-right: 0.75rem;
  cursor: pointer;
}

.file-limits {
  color: var(--muted);
  font-size: 0.8125rem;
  margin: 0.375rem 0 0;
}

/* The submission form's stand-in for a file input: the upload comes after
   verification, and this is where the form says so. */
.file-note {
  color: var(--muted);
  font-size: 0.875rem;
  padding: 0.625rem 0.75rem;
  background: var(--surface);
  border: 1px dashed var(--line);
  border-radius: var(--radius);
  margin: 0;
}

/* The chevron is drawn here rather than left to the platform, because a
   native select arrow is the one control that makes a styled form look
   half-styled. `img-src data:` covers it; nothing is fetched. */
select {
  appearance: none;
  padding-right: 2.25rem;
  background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16' fill='none' stroke='%239a9aa8' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M4 6l4 4 4-4'/%3E%3C/svg%3E");
  background-repeat: no-repeat;
  background-position: right 0.75rem center;
  background-size: 1rem;
}

input:hover, select:hover, textarea:hover { border-color: var(--muted); }

/* One focus treatment everywhere: a ring outside the border rather than an
   outline on it, so nothing shifts by a pixel when a field takes focus. */
input:focus-visible, select:focus-visible, textarea:focus-visible,
button:focus-visible, a:focus-visible {
  outline: none;
  border-color: var(--ring);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--ring) 35%, transparent);
}

/* --- checkboxes --------------------------------------------------------- */

.choices { display: grid; gap: 0.625rem; margin-top: 0.25rem; }

.ack label, .choice {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 0.625rem;
  align-items: start;
  font-family: var(--font-sans);
  font-size: 0.9375rem;
  font-weight: 400;
  letter-spacing: 0;
  cursor: pointer;
}

input[type=checkbox], input[type=radio] {
  appearance: none;
  width: 1.125rem;
  height: 1.125rem;
  margin: 0.19rem 0 0;
  flex: none;
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: 4px;
  cursor: pointer;
}
input[type=radio] { border-radius: 50%; }

input[type=checkbox]:checked, input[type=radio]:checked {
  background: var(--accent);
  border-color: var(--accent);
}
input[type=checkbox]:checked {
  background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16' fill='none' stroke='%23ffffff' stroke-width='2.25' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M3.5 8.5l3 3 6-6'/%3E%3C/svg%3E");
  background-size: 0.875rem;
  background-position: center;
  background-repeat: no-repeat;
}

/* --- steps -------------------------------------------------------------- */

fieldset {
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 1.5rem;
  margin: 0 0 2rem;
  background: color-mix(in srgb, var(--surface) 45%, transparent);
}

legend {
  font-family: var(--font-mono);
  font-size: 0.75rem;
  font-weight: 500;
  text-transform: uppercase;
  letter-spacing: 0.08em;
  color: var(--muted);
  padding: 0 0.5rem;
}

/* --- what went wrong ---------------------------------------------------- */

/* The signal colour marks and never fills. A field that failed gets a rule
   and a word, not a coloured panel — the palette has one accent and treats
   amber as a called-out line rather than a background. */
[aria-invalid=true] { border-color: var(--signal); }

.error {
  color: var(--signal);
  font-family: var(--font-mono);
  font-size: 0.8125rem;
  margin: 0.5rem 0 0;
}

.errors {
  border-left: 2px solid var(--signal);
  padding: 0.25rem 0 0.25rem 1rem;
  margin: 0 0 2rem;
}
.errors p { margin: 0 0 0.25rem; font-weight: 500; }
.errors ul { margin: 0; padding-left: 1.1rem; color: var(--muted); font-size: 0.9375rem; }

/* --- submit ------------------------------------------------------------- */

button {
  font-family: var(--font-sans);
  font-size: 0.9375rem;
  font-weight: 500;
  padding: 0.6875rem 1.5rem;
  border: 1px solid transparent;
  border-radius: var(--radius);
  background: var(--accent);
  color: var(--on-accent);
  cursor: pointer;
  transition: opacity .12s ease;
}
button:hover { opacity: .88; }
button:active { opacity: .96; }

a { color: var(--accent); text-underline-offset: 2px; }

/* The confirmation and error pages share the shell, so they get the same
   measure and rhythm without a second stylesheet. */
main > h1:only-of-type + p { color: var(--muted); }

/* The site header: mark on the left, account on the right. Present on the
   gate's own pages and on rendered forms; never on artifact origins, whose
   bytes are content-addressed and not ours to decorate. A full-width band
   with its own rule underneath, so the chrome reads as chrome and the page
   starts where the border ends. */
header.site {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0.875rem 2rem;
  border-bottom: 1px solid var(--line);
  color: var(--text);
}
/* The mark wears the brand's accent — it sits on the page ground, where the
   theme's own light/dark accent pair is exactly the contrast rule. */
header.site a.mark { color: var(--accent); display: inline-flex; }
header.site a.mark:hover { opacity: .8; }
header.site > nav { display: flex; gap: 1rem; align-items: center; }
header.site > nav a {
  font-size: 0.875rem;
  color: var(--muted);
  text-decoration: none;
}
header.site > nav a:hover { color: var(--accent); text-decoration: underline; }

/* The account dropdown is a <details>: open/close is the browser's, no
   script anywhere. */
.account-menu { position: relative; }
.account-menu > summary {
  list-style: none;
  cursor: pointer;
  font-family: var(--font-mono);
  font-size: 0.875rem;
  padding: 0.375rem 0.75rem;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  background: var(--surface);
}
.account-menu > summary::-webkit-details-marker { display: none; }
.account-menu[open] > summary { border-color: var(--accent); }
.account-menu > .menu {
  position: absolute;
  right: 0;
  margin-top: 0.375rem;
  min-width: 14rem;
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  padding: 0.75rem;
  z-index: 1;
}
.account-menu .menu p {
  color: var(--muted);
  font-size: 0.8125rem;
  margin: 0 0 0.5rem;
  overflow-wrap: anywhere;
}
.account-menu .menu nav a { display: block; padding: 0.25rem 0; font-size: 0.875rem; }
.account-menu .menu form { margin: 0.5rem 0 0; }
.account-menu .menu button {
  width: 100%;
  padding: 0.4375rem 0.75rem;
  font-size: 0.875rem;
  background: var(--ground);
  color: var(--text);
  border-color: var(--line);
}

/* Tables — the account page's artifacts and machines. */
table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.875rem;
  margin: 0.75rem 0 1.5rem;
}
th {
  text-align: left;
  color: var(--muted);
  font-weight: 500;
  padding: 0.375rem 0.75rem 0.375rem 0;
  border-bottom: 1px solid var(--line);
}
td {
  padding: 0.5rem 0.75rem 0.5rem 0;
  border-bottom: 1px solid var(--line);
  vertical-align: middle;
  overflow-wrap: anywhere;
}
td form { margin: 0; display: inline; }
td button { padding: 0.25rem 0.625rem; font-size: 0.8125rem; }

@media (max-width: 30rem) {
  main { padding: 2.5rem 1.25rem 4rem; }
  h1 { font-size: 1.5rem; }
  header.site { padding: 1rem 1.25rem 0; }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValidationError;
    use serde_json::json;

    fn t() -> RequestType {
        RequestType::from_toml(
            r#"
id = "speaking"
version = 1
title = "Invite a talk"
description = "Tell me about the event."

[[fields]]
name = "requester_email"
label = "Your email"
help = "So I can reply."
kind = "email"
required = true

[[fields]]
name = "event_date"
label = "Date"
kind = "date"
min = "2026-01-01"
required = true

[[fields]]
name = "format"
label = "Format"
kind = "select"
required = true
options = [
  { value = "in_person", label = "In person" },
  { value = "remote", label = "Remote" },
]

[[fields]]
name = "travel_city"
label = "Flying from"
kind = "text"
max_length = 80
required = true
show_when = { field = "format", op = "eq", value = "in_person" }

[[acknowledgments]]
id = "consent"
label = "I understand this is a request"
info_link = "https://example.edu/policy"
"#,
        )
        .unwrap()
    }

    #[test]
    fn html5_constraints_come_off_the_manifest_so_the_browser_validates_natively() {
        let page = t().form(&FormOptions::default());
        let html = &page.html;
        assert!(html.contains(r#"type="email""#));
        assert!(html.contains(r#"maxlength="254""#));
        assert!(html.contains(r#"type="date""#) && html.contains(r#"min="2026-01-01""#));
        assert!(html.contains(r#"maxlength="80""#));
        assert!(html.contains(r#"<option value="in_person">In person</option>"#));
        // A required select starts unchosen rather than defaulting to whatever
        // is first: a default answer to a question nobody read is worse.
        assert!(html.contains(r#"<option value="">Choose…</option>"#));
        // `novalidate` is a boolean attribute: emitting it with *any* value —
        // including "false" — turns off the whole constraint layer above.
        assert!(
            !html.contains("novalidate"),
            "browser validation was disabled"
        );
        // Four required fields plus the acknowledgment, which is always
        // required — that is what makes it an acknowledgment.
        assert_eq!(html.matches(" required").count(), 5);
    }

    /// The strictest CSP has to hold with nothing relaxed, so nothing is
    /// inline — the conditions ship as JSON data, not as code.
    #[test]
    fn nothing_executable_is_inline() {
        let page = t().form(&FormOptions::default());
        assert!(page.html.contains(r#"<script src="form.js" defer>"#));
        assert!(page
            .html
            .contains(r#"<script type="application/json" id="conditions">"#));
        assert!(
            !page.html.contains("onclick=") && !page.html.contains("onchange="),
            "no inline handlers"
        );
        assert!(page
            .html
            .contains(r#"<link rel="stylesheet" href="form.css">"#));
        assert_eq!(page.assets().len(), 2);
    }

    /// A `</script>` inside a JSON island ends the element whatever the quoting
    /// — the one escape a JSON encoder does not do for you.
    #[test]
    fn a_closing_script_tag_in_a_condition_cannot_break_out_of_the_json_island() {
        let t = RequestType::from_toml(
            r#"
id = "x"
version = 1
title = "x"
[[fields]]
name = "a"
label = "A"
kind = "text"
max_length = 10
[[fields]]
name = "b"
label = "B"
kind = "bool"
show_when = { field = "a", op = "eq", value = "</script><script>alert(1)</script>" }
"#,
        )
        .unwrap();
        let html = t.form(&FormOptions::default()).html;
        assert!(
            !html.contains("</script><script>alert"),
            "the island was broken out of"
        );
        assert!(html.contains(r"<\/script>"));
    }

    /// Pre-filled values are a stranger's rejected submission being shown back,
    /// which is the one place in this file where escaping is load-bearing.
    #[test]
    fn a_hostile_prefill_is_escaped_in_every_control() {
        let options = FormOptions {
            values: json!({
            "requester_email": "\"><script>alert(1)</script>",
            "travel_city": "\"><img src=x onerror=alert(1)>",
                "format": "in_person",
            })
            .as_object()
            .unwrap()
            .clone(),
            ..FormOptions::default()
        };
        let html = t().form(&options).html;
        // The test is that no *tag* survives, not that the words do — escaping
        // neutralises the delimiters and leaves the prose, which is the point.
        assert!(!html.contains("<script>alert(1)"), "a script tag survived");
        assert!(!html.contains("<img src=x"), "an img tag survived");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        // And the `">` prefix — the actual attribute-escape attempt — was
        // neutralised rather than merely surviving as text.
        assert!(
            html.contains("&quot;&gt;&lt;script"),
            "the quote was not escaped"
        );
        assert!(html.contains("&quot;&gt;&lt;img"));
    }

    #[test]
    fn errors_are_shown_twice_on_purpose_and_wired_for_a_screen_reader() {
        let options = FormOptions {
            errors: vec![ValidationError {
                field: "event_date".into(),
                message: "this cannot be earlier than 2026-01-01".into(),
            }],
            ..FormOptions::default()
        };
        let html = t().form(&options).html;
        // The summary a screen reader announces on load...
        assert!(html.contains(r#"<div class="errors" role="alert">"#));
        assert!(html.contains(r##"<a href="#event_date">Date</a>"##));
        // ...and the message beside the input, referenced from it.
        assert!(html.contains(r#"aria-invalid="true""#));
        assert!(html.contains(r#"aria-describedby="event_date-error""#));
        assert!(html.contains(r#"<p class="error" id="event_date-error">"#));
    }

    #[test]
    fn help_text_is_referenced_from_its_input_rather_than_left_floating() {
        let html = t().form(&FormOptions::default()).html;
        assert!(html.contains(r#"aria-describedby="requester_email-help""#));
        assert!(html.contains(r#"<p class="help" id="requester_email-help">So I can reply."#));
    }

    /// An external font would be an external reference, and the vendoring gate
    /// fails a publish on one.
    #[test]
    fn the_generated_assets_reference_nothing_off_the_origin() {
        // Every built-in theme, because the stylesheet is now a function of
        // which one was asked for — and a palette is exactly the sort of file
        // somebody would paste a hosted font import into.
        for theme in crate::theme::BUILT_IN {
            let page = t().form(&FormOptions {
                theme,
                ..FormOptions::default()
            });
            for (name, contents) in page.assets() {
                // `url()` is permitted only for `data:`. The select chevron and
                // the checkbox tick are drawn inline, which is bytes the page
                // already holds rather than a fetch; anything else in a `url()`
                // is a resource loaded on render, which is what this test is
                // for. The rule used to be "no `url(` at all", a proxy that
                // would have banned the chevron along with the CDN.
                let mut outside = String::new();
                let mut rest = contents;
                while let Some(at) = rest.find("url(") {
                    outside.push_str(&rest[..at]);
                    let after = &rest[at + 4..];
                    let target = after.trim_start_matches(['"', '\'']);
                    assert!(
                        target.starts_with("data:"),
                        "{name} loads something under `{}`: url({}…",
                        theme.name,
                        &target[..target.len().min(40)]
                    );
                    // Skipped, not scanned. A data URI's *contents* are inert —
                    // the chevron's SVG declares `xmlns="http://www.w3.org/2000/svg"`,
                    // which is an identifier no browser fetches, and the
                    // vendoring gate keeps the same namespace list for the same
                    // reason.
                    // Resume relative to `after`, never to `contents`. `at` is
                    // an offset into `rest`, so a cursor derived from it and
                    // applied to `contents` agrees only while `rest` *is*
                    // `contents` — one pass. After the first `url(`, `rest` is
                    // a suffix, the cursor lands short by that suffix's start,
                    // and the scan walks backwards onto a `url(` it already
                    // passed. Two data URIs in one stylesheet — which is every
                    // built-in theme, chevron and tick — then loop forever with
                    // `outside` growing until the machine OOMs.
                    rest = match after.find(')') {
                        Some(i) => &after[i + 1..],
                        None => "",
                    };
                }
                outside.push_str(rest);

                // Comments are stripped before the scan, because a comment
                // loads nothing and the structural sheet's own header says it
                // has "No @import" — prose the substring check would otherwise
                // read as the directive it is promising not to use. Rewording
                // the comment would pass too, and would leave the trap armed
                // for whoever next writes `@import` while explaining its
                // absence. Safe after the `url()` pass rather than before it:
                // `outside` no longer holds any data URI, so a `/*` here can
                // only be a real comment.
                let mut scanned = String::with_capacity(outside.len());
                let mut tail = outside.as_str();
                while let Some(open) = tail.find("/*") {
                    scanned.push_str(&tail[..open]);
                    tail = match tail[open + 2..].find("*/") {
                        Some(close) => &tail[open + 2 + close + 2..],
                        None => "",
                    };
                }
                scanned.push_str(tail);

                for needle in ["http://", "https://", "//cdn", "@import"] {
                    assert!(
                        !scanned.contains(needle),
                        "{name} contains an external reference under `{}`: {needle}",
                        theme.name
                    );
                }
            }
        }

        let page = t().form(&FormOptions::default());
        // The one absolute URL in the page is the acknowledgment's info link,
        // which is a link for a human rather than a resource the page loads.
        assert!(page.html.contains(r#"rel="noopener noreferrer""#));
    }

    #[test]
    fn a_conditional_field_is_marked_for_the_evaluator_and_shown_without_it() {
        let html = t().form(&FormOptions::default()).html;
        assert!(html.contains(r#"data-field="travel_city" data-conditional="true""#));
        // Not `hidden` in the markup: with JavaScript off, showing an optional
        // question beats hiding a required one and not saying why.
        assert!(!html.contains(r#"data-field="travel_city" data-conditional="true" hidden"#));
    }

    #[test]
    fn only_the_named_step_renders_when_one_is_asked_for() {
        let t = RequestType::from_toml(
            r#"
id = "two"
version = 1
title = "Two"
[[fields]]
name = "a"
label = "A"
kind = "bool"
[[fields]]
name = "b"
label = "B"
kind = "bool"
[[steps]]
id = "one"
title = "First"
fields = ["a"]
[[steps]]
id = "two"
title = "Second"
fields = ["b"]
"#,
        )
        .unwrap();
        let options = FormOptions {
            step: Some("two".into()),
            ..FormOptions::default()
        };
        let html = t.form(&options).html;
        assert!(html.contains(r#"id="step-two""#));
        assert!(!html.contains(r#"id="step-one""#));
    }

    /// A file field renders as a note on the submission form and as an input
    /// only on the upload page — and `enctype` follows the input, so the
    /// public form's bytes never claim to carry what they cannot.
    #[test]
    fn a_file_field_is_a_note_until_the_upload_page() {
        let t = RequestType::from_toml(
            r#"
id = "t"
version = 1
title = "t"
[[fields]]
name = "cv"
label = "Your CV"
kind = "file"
accept = ["pdf", "jpg"]
required = true
"#,
        )
        .unwrap();

        let submission = t.form(&FormOptions::default()).html;
        assert!(submission.contains("file-note"));
        assert!(submission.contains("after verifying your email"));
        assert!(!submission.contains(r#"type="file""#));
        assert!(!submission.contains("enctype"));

        let upload = t
            .form(&FormOptions {
                file_inputs: true,
                ..FormOptions::default()
            })
            .html;
        assert!(upload.contains(r#"type="file""#));
        assert!(upload.contains(r#"enctype="multipart/form-data""#));
        assert!(upload.contains(r#"accept=".pdf,application/pdf,.jpg,.jpeg,image/jpeg""#));
        assert!(upload.contains("up to 8 MB"));

        // A type with no file fields is byte-identical whichever mode renders
        // it: `file_inputs` must change nothing it does not own.
        let plain = RequestType::from_toml(
            r#"
id = "p"
version = 1
title = "p"
[[fields]]
name = "email"
label = "Email"
kind = "email"
"#,
        )
        .unwrap();
        let a = plain.form(&FormOptions::default()).html;
        let b = plain
            .form(&FormOptions {
                file_inputs: true,
                ..FormOptions::default()
            })
            .html;
        assert_eq!(a, b);
    }
}
