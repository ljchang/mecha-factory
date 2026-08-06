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

use crate::{escape, Acknowledgment, Condition, Field, FieldKind, RequestType, Step};

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
}

impl Default for FormOptions {
    fn default() -> Self {
        FormOptions {
            action: "".into(),
            token: None,
            values: serde_json::Map::new(),
            errors: Vec::new(),
            step: None,
        }
    }
}

/// A rendered form: the page, plus the files it references.
pub struct FormPage {
    pub html: String,
    pub script: &'static str,
    pub style: &'static str,
}

impl FormPage {
    /// Filename → contents, for whoever is writing the bundle out.
    pub fn assets(&self) -> [(&'static str, &'static str); 2] {
        [("form.js", self.script), ("form.css", self.style)]
    }
}

impl RequestType {
    /// Render this request type as a form.
    pub fn form(&self, options: &FormOptions) -> FormPage {
        let mut body = String::new();
        body.push_str(&format!("<h1>{}</h1>\n", escape(&self.title)));
        if let Some(description) = &self.description {
            body.push_str(&format!("<p class=\"intro\">{}</p>\n", escape(description)));
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
                    escape(&error.field),
                    escape(label),
                    escape(&error.message)
                ));
            }
            body.push_str("</ul></div>\n");
        }

        // No `novalidate`, and deliberately not `novalidate="false"` either —
        // it is a boolean attribute, so *any* value means present, and writing
        // it out to say "please do validate" turns the entire HTML5 constraint
        // layer off. Found by opening the page rather than by reading the code.
        body.push_str(&format!(
            "<form method=\"post\" action=\"{}\">\n",
            escape(&options.action)
        ));
        if let Some(token) = &options.token {
            body.push_str(&format!(
                "<input type=\"hidden\" name=\"_token\" value=\"{}\">\n",
                escape(token)
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
        body.push_str("<script src=\"form.js\" defer></script>\n");

        let html = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n\
             <meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
             <title>{}</title>\n\
             <link rel=\"stylesheet\" href=\"form.css\">\n\
             </head>\n<body>\n<main>\n{}</main>\n</body>\n</html>\n",
            escape(&self.title),
            body
        );

        FormPage {
            html,
            script: FORM_JS,
            style: FORM_CSS,
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
            escape(&step.id),
            escape(&step.id),
            escape(&step.title)
        );
        if let Some(description) = &step.description {
            out.push_str(&format!("<p>{}</p>\n", escape(description)));
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
        let name = escape(&field.name);
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
                    .map(escape)
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
                        escape(&choice.value),
                        escape(&choice.label)
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
                        escape(&choice.value),
                        escape(&choice.label)
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
                                .map(|p| format!(" pattern=\"{}\"", escape(p)))
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
                                .map(|m| format!(" min=\"{}\"", escape(m)))
                                .unwrap_or_default(),
                            max.as_ref()
                                .map(|m| format!(" max=\"{}\"", escape(m)))
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
                            serde_json::Value::String(s) => escape(s),
                            other => escape(&other.to_string()),
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
            escape(&field.label),
            if field.required {
                " <span class=\"req\" aria-hidden=\"true\">*</span>"
            } else {
                ""
            }
        );
        if let Some(help) = &field.help {
            out.push_str(&format!(
                "<p class=\"help\" id=\"{name}-help\">{}</p>\n",
                escape(help)
            ));
        }
        out.push_str(&control);
        out.push('\n');
        if let Some(error) = error {
            out.push_str(&format!(
                "<p class=\"error\" id=\"{name}-error\">{}</p>\n",
                escape(&error.message)
            ));
        }
        out.push_str("</div>\n");
        out
    }
}

fn render_acknowledgment(ack: &Acknowledgment, options: &FormOptions) -> String {
    let id = escape(&ack.id);
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
        escape(&ack.label)
    ));
    if let Some(description) = &ack.description {
        out.push_str(&format!("<p class=\"help\">{}</p>\n", escape(description)));
    }
    if let Some(link) = &ack.info_link {
        // `rel="noopener noreferrer"` because the destination is named in our
        // own manifest but opens in the reader's browser, and `target="_blank"`
        // without it hands the opener a handle back.
        out.push_str(&format!(
            "<p class=\"help\"><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">\
             What this means</a></p>\n",
            escape(link)
        ));
    }
    if let Some(error) = error {
        out.push_str(&format!(
            "<p class=\"error\" id=\"{id}-error\">{}</p>\n",
            escape(&error.message)
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

const FORM_CSS: &str = r#"/* Generated by mecha-manifest. One column, system fonts, no imports —
   an external font is an external reference, and the vendoring gate fails a
   publish on one. */
:root { color-scheme: light dark; --fg: #1a1a1a; --bg: #fff; --muted: #5a5a5a;
        --line: #d8d8d8; --err: #a3231b; --accent: #1f5fa9; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #e8e8e8; --bg: #16181c; --muted: #9aa0a6; --line: #383c42;
          --err: #ff8a80; --accent: #7cb2ff; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); line-height: 1.55;
       font-family: system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 42rem; margin: 0 auto; padding: 2rem 1.25rem 4rem; }
h1 { font-size: 1.6rem; line-height: 1.25; margin: 0 0 .5rem; }
.intro { color: var(--muted); margin-top: 0; }
.field { margin: 1.5rem 0; }
.field[hidden] { display: none; }
label { display: block; font-weight: 600; margin-bottom: .35rem; }
.ack label, .choice { font-weight: 400; display: flex; gap: .5rem;
                      align-items: flex-start; }
.help { color: var(--muted); font-size: .9rem; margin: .2rem 0 .4rem; }
.req { color: var(--err); }
input[type=text], input[type=email], input[type=url], input[type=date],
input[type=number], select, textarea {
  width: 100%; padding: .55rem .65rem; font: inherit; color: inherit;
  background: transparent; border: 1px solid var(--line); border-radius: 6px;
}
input:focus-visible, select:focus-visible, textarea:focus-visible,
button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
[aria-invalid=true] { border-color: var(--err); }
.error { color: var(--err); font-size: .9rem; margin: .35rem 0 0; }
.errors { border: 1px solid var(--err); border-radius: 6px; padding: .75rem 1rem;
          margin: 1.5rem 0; }
.errors ul { margin: .5rem 0 0; padding-left: 1.2rem; }
.choices { display: grid; gap: .4rem; }
fieldset { border: 1px solid var(--line); border-radius: 8px; padding: 0 1rem 1rem;
           margin: 2rem 0; }
legend { font-weight: 600; padding: 0 .4rem; }
button { font: inherit; font-weight: 600; padding: .6rem 1.4rem; border-radius: 6px;
         border: 1px solid transparent; background: var(--accent); color: #fff;
         cursor: pointer; }
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
        let page = t().form(&FormOptions::default());
        for (name, contents) in page.assets() {
            for needle in ["http://", "https://", "//cdn", "@import", "url("] {
                assert!(
                    !contents.contains(needle),
                    "{name} contains an external reference: {needle}"
                );
            }
        }
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
}
