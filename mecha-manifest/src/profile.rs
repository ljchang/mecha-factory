//! The personal public surface, as data: a profile, and the boards it wears.
//!
//! Two records travel between a person's machine and the box. A [`Profile`] is
//! who they are — a name, a line about themselves, some links. A
//! [`Board`] is a page: the **hangar** (`slug` absent) lists everything public
//! and is wired from the inventory, and a **switchboard** (`slug` present) is
//! a hand-patched set of lines the owner names themselves.
//!
//! Three rules decide the shape, and each is a bug if undone.
//!
//! **A profile is data, never a page.** There is no field here that accepts
//! HTML, markdown, or CSS, and there is deliberately no escape hatch for one.
//! `theme.rs` already makes this argument for forms — *"a theme is tokens,
//! never rules"* — and the second reason is sharper: these render on the gate,
//! where the `__Host-` session cookie lives, so author-controlled markup would
//! walk straight through the reasoning in `http/account.rs`'s module doc.
//! Everything here is a typed scalar that the renderer escapes.
//!
//! **An entry is a reference, not a URL.** `kind = "form"` plus an `id` is
//! something the server can resolve against its own tables and *prove* exists,
//! belongs to this user, and is public. A URL can only be trusted. The one
//! exception is [`EntryKind::Link`], which exists because a real person's page
//! links to their lab site — and it is rendered with its host visible, which
//! is the same distinction the publish gate already draws between `<a href>`
//! (navigation, never a finding) and `<img src>` (the page reaching out).
//!
//! **A URL is `http`/`https` or it is refused.** Not a filter on known-bad
//! schemes: an allowlist, because `javascript:`, `data:` and `vbscript:` are
//! the three anyone remembers and the list of what a browser will execute is
//! not ours to keep current.

use serde::{Deserialize, Serialize};

use crate::{ManifestError, Result};

/// Slugs a board may never claim, because the gate serves something else
/// there — or will.
///
/// Reserved *before* the first slug exists, which is the only time it can be:
/// a slug goes in an email signature and can never be taken back, the same
/// argument that makes a handle unreusable. Retrofitting this list means
/// either breaking a live URL or never using the route.
pub const RESERVED_SLUGS: [&str; 13] = [
    "v", "b", "f", "s", "p", "g", "a", "account", "signin", "signup", "admin", "view", "slides",
];

/// The longest a single-line field may be. Generous for a tagline, short
/// enough that a page cannot be turned into an essay nobody asked for.
const LINE_MAX: usize = 160;
/// The longest the one multi-line field may be.
const BIO_MAX: usize = 600;
/// Boards are meant to be short. A switchboard with forty lines is a hangar
/// that somebody built by hand.
const MAX_ENTRIES: usize = 24;
const MAX_LINKS: usize = 12;

/// Where a link points, when we know enough to give it an icon.
///
/// The named kinds earn a shipped inline SVG; [`LinkKind::Other`] renders
/// with its hostname visible and no icon. An icon is inline because an
/// `<img src="https://…">` is what the publish gate fails a bundle for, and
/// the CSP would block it regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    #[default]
    Other,
    Website,
    Github,
    Scholar,
    Orcid,
    Mastodon,
    Bluesky,
    Linkedin,
}

/// One link out of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    #[serde(default)]
    pub kind: LinkKind,
    /// What to call it. Absent means the renderer uses the kind's own name,
    /// or the hostname for [`LinkKind::Other`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub url: String,
}

/// Who somebody is, for the top of their pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Whether the **hangar** is served. Default false: an existing account
    /// that never asked for a public index must not acquire one by upgrading.
    ///
    /// It governs the hangar alone. A switchboard whose URL is in somebody's
    /// email signature keeps working when this is off — those are separate
    /// publications with separate audiences, and conflating them would make
    /// "hide my index" silently break a link a stranger is holding.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// An IANA name, checked against the real database. Rendered beside a
    /// booking line, where "I am in America/New_York" is the difference
    /// between a useful page and one that wastes somebody's morning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

/// What a line on a board points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Booking,
    Form,
    Bundle,
    Poll,
    /// The honest escape hatch: somewhere that is not ours. Rendered with its
    /// host visible, always.
    Link,
}

impl EntryKind {
    /// Whether this kind names something the server can resolve.
    pub fn is_reference(&self) -> bool {
        !matches!(self, EntryKind::Link)
    }
}

/// One line on a switchboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: EntryKind,
    /// The artifact's id, for every kind but [`EntryKind::Link`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The destination, for [`EntryKind::Link`] only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blurb: Option<String>,
}

/// Which of the two wirings a board is.
///
/// Derived from `slug` rather than stored beside it, so the two can never
/// disagree — a board carrying `kind = "hangar"` and a slug would be a
/// question with two answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardKind {
    /// Lines wired automatically: everything public, grouped.
    Hangar,
    /// Lines patched by hand.
    Switchboard,
}

/// One page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Board {
    /// Absent for the hangar at `/@{handle}`; present for a switchboard at
    /// `/@{handle}/{slug}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    #[serde(default, rename = "entry", skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,
}

impl Board {
    pub fn kind(&self) -> BoardKind {
        match self.slug {
            None => BoardKind::Hangar,
            Some(_) => BoardKind::Switchboard,
        }
    }
}

// ---- parsing and validation ---------------------------------------------

impl Profile {
    pub fn from_toml(text: &str) -> Result<Profile> {
        let profile: Profile = toml::from_str(text)?;
        profile.check()?;
        Ok(profile)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| ManifestError::invalid(format!("serialising a profile: {e}")))
    }

    pub fn check(&self) -> Result<()> {
        line(&self.display_name, "display_name")?;
        line(&self.tagline, "tagline")?;
        line(&self.location, "location")?;
        prose(&self.bio, "bio")?;
        check_timezone(&self.timezone)?;
        if self.links.len() > MAX_LINKS {
            return Err(ManifestError::invalid(format!(
                "a profile carries at most {MAX_LINKS} links"
            )));
        }
        for link in &self.links {
            line(&link.label, "a link's label")?;
            check_url(&link.url)?;
        }
        Ok(())
    }
}

impl Board {
    pub fn from_toml(text: &str) -> Result<Board> {
        let board: Board = toml::from_str(text)?;
        board.check()?;
        Ok(board)
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| ManifestError::invalid(format!("serialising a board: {e}")))
    }

    pub fn check(&self) -> Result<()> {
        if let Some(slug) = &self.slug {
            check_slug(slug)?;
        }
        line(&self.heading, "heading")?;
        prose(&self.intro, "intro")?;

        if self.kind() == BoardKind::Hangar && !self.entries.is_empty() {
            return Err(ManifestError::invalid(
                "a hangar's lines are wired from what is public, so it declares none — give this \
                 board a slug to patch lines by hand",
            ));
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err(ManifestError::invalid(format!(
                "a board carries at most {MAX_ENTRIES} lines"
            )));
        }
        for entry in &self.entries {
            entry.check()?;
        }
        Ok(())
    }
}

impl Entry {
    fn check(&self) -> Result<()> {
        if self.label.trim().is_empty() {
            return Err(ManifestError::invalid(
                "every line needs a label: it is the button somebody clicks",
            ));
        }
        line(&Some(self.label.clone()), "a line's label")?;
        prose(&self.blurb, "a line's blurb")?;

        // The exclusivity is the whole point of `kind`: a reference is
        // something the server resolves and can prove, a link is something it
        // can only pass on. A line carrying both would be asking which of the
        // two it is at render time, which is the question this refuses.
        match (self.kind.is_reference(), &self.id, &self.url) {
            (true, Some(id), None) => crate::valid_id(id, "a line's id"),
            (true, _, Some(_)) => Err(ManifestError::invalid(format!(
                "a `{:?}` line names an id, never a url — the server resolves it, which is what \
                 lets a dark line be noticed instead of served",
                self.kind
            ))),
            (true, None, None) => Err(ManifestError::invalid(format!(
                "a `{:?}` line needs an id",
                self.kind
            ))),
            (false, None, Some(url)) => check_url(url),
            (false, Some(_), _) => Err(ManifestError::invalid(
                "a `link` line carries a url, never an id",
            )),
            (false, None, None) => Err(ManifestError::invalid("a `link` line needs a url")),
        }
    }
}

fn line(value: &Option<String>, what: &str) -> Result<()> {
    let Some(text) = value else { return Ok(()) };
    if text.chars().count() > LINE_MAX {
        return Err(ManifestError::invalid(format!(
            "{what} is longer than {LINE_MAX} characters"
        )));
    }
    if text.chars().any(|c| c.is_control()) {
        return Err(ManifestError::invalid(format!(
            "{what} is one line, so it carries no newlines or control characters"
        )));
    }
    Ok(())
}

fn prose(value: &Option<String>, what: &str) -> Result<()> {
    let Some(text) = value else { return Ok(()) };
    if text.chars().count() > BIO_MAX {
        return Err(ManifestError::invalid(format!(
            "{what} is longer than {BIO_MAX} characters"
        )));
    }
    // Newlines are the point of a prose field; everything else that steers a
    // terminal or a parser is not.
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r')
    {
        return Err(ManifestError::invalid(format!(
            "{what} carries a control character"
        )));
    }
    Ok(())
}

/// A zone that tracks its region's rules, which is narrower than "parses".
///
/// The IANA database still carries the legacy fixed-offset zones — `EST`,
/// `MST`, `PST8PDT` — and `chrono_tz` parses them happily. They are almost
/// never what somebody means: a person writing `EST` means the US Eastern
/// region, and gets times an hour wrong from March to November. That is the
/// worst shape of wrong, because it stays internally consistent and reads as
/// correct.
///
/// So the rule is `Region/City`, with `UTC` allowed because it is the one
/// bare name that is honestly fixed. `Etc/GMT+5` passes, and that is fine —
/// nobody types it by accident.
fn check_timezone(name: &Option<String>) -> Result<()> {
    let Some(tz) = name else { return Ok(()) };
    if tz.parse::<chrono_tz::Tz>().is_err() {
        return Err(ManifestError::invalid(format!(
            "`{tz}` is not an IANA timezone name — try `America/New_York`"
        )));
    }
    if tz != "UTC" && !tz.contains('/') {
        return Err(ManifestError::invalid(format!(
            "`{tz}` is a fixed-offset zone, not a region: it does not follow daylight saving, so \
             half the year it renders times that are wrong and look right. Name the region — \
             `America/New_York` rather than `EST`"
        )));
    }
    Ok(())
}

/// An allowlist, never a blocklist. `javascript:`, `data:` and `vbscript:`
/// are the three anybody thinks to exclude, and keeping a current list of
/// everything a browser will execute is not a job worth accepting.
fn check_url(url: &str) -> Result<()> {
    let lowered = url.trim().to_ascii_lowercase();
    if !(lowered.starts_with("https://") || lowered.starts_with("http://")) {
        return Err(ManifestError::invalid(format!(
            "`{url}` is not an http(s) URL, and only http(s) is linked"
        )));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ManifestError::invalid(format!(
            "`{url}` carries whitespace or a control character"
        )));
    }
    if url.len() > 400 {
        return Err(ManifestError::invalid("a URL longer than 400 characters"));
    }
    Ok(())
}

fn check_slug(slug: &str) -> Result<()> {
    crate::valid_id(slug, "a board's slug")?;
    if RESERVED_SLUGS.contains(&slug) {
        return Err(ManifestError::invalid(format!(
            "`{slug}` is reserved for the gate's own routes. A slug can never be reissued once \
             somebody has it, so the reserved names are refused rather than taken back later"
        )));
    }
    Ok(())
}

// ---- the merge ----------------------------------------------------------

/// What a push did to a record that the browser had also been editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// The record to store, as TOML.
    pub merged: String,
    /// Fields where both sides had changed and the pushed file won. Named so
    /// the push can say what it overwrote — a silent clobber is the failure
    /// this whole mechanism exists to avoid.
    pub overwritten: Vec<String>,
    /// True when the incoming text was stored byte for byte, comments and
    /// all. See [`merge_push`].
    pub verbatim: bool,
}

/// Fold a pushed file into a record the browser may also have edited.
///
/// Three texts, which is what makes this a merge rather than a replace:
///
/// ```text
///   baseline    the TOML exactly as last received from a push
///   effective   what the page renders now (baseline + browser edits)
///   incoming    the file being pushed
/// ```
///
/// A field that changed in `incoming` is applied. If the browser had changed
/// that same field since `baseline`, the pushed value wins and the field is
/// named in [`Merge::overwritten`] — "TOML wins" means *TOML wins conflicts*,
/// not "the last push clobbers", and the difference is what makes editing in
/// a browser worth doing at all.
///
/// **The common case is byte-for-byte.** When the browser has changed
/// nothing (`effective == baseline`), the incoming text is stored exactly as
/// it arrived — comments, ordering and spacing intact. Re-serialising is
/// lossy, so it is confined to the case where a merge genuinely happened;
/// somebody who never opens the cockpit never loses a comment.
pub fn merge_push(baseline: &str, effective: &str, incoming: &str) -> Result<Merge> {
    if baseline.trim() == effective.trim() {
        return Ok(Merge {
            merged: incoming.to_string(),
            overwritten: Vec::new(),
            verbatim: true,
        });
    }

    let table = |text: &str, what: &str| -> Result<toml::value::Table> {
        match toml::from_str::<toml::Value>(text) {
            Ok(toml::Value::Table(t)) => Ok(t),
            Ok(_) => Err(ManifestError::invalid(format!(
                "{what} is not a TOML table"
            ))),
            Err(e) => Err(ManifestError::invalid(format!("{what}: {e}"))),
        }
    };
    let base = table(baseline, "the stored baseline")?;
    let mut eff = table(effective, "the stored record")?;
    let inc = table(incoming, "the pushed file")?;

    // **No baseline means the file has never spoken about this record**, so
    // there is nothing to reconcile and the file becomes the record whole.
    //
    // The alternative was to treat every stored key as an addition the file
    // never knew about and keep it — non-destructive, and it leaves a field
    // written in the cockpit removable only by adding it to the file and then
    // deleting it again, which nobody would ever guess. This way deletions
    // work from the first push, `drifted` means something, and the cost is
    // paid where it can be seen: every displaced field is named, and the
    // cockpit says the record exists in no file at all.
    if base.is_empty() && !eff.is_empty() {
        let mut displaced: Vec<String> = eff
            .keys()
            .filter(|key| inc.get(*key) != eff.get(*key))
            .cloned()
            .collect();
        displaced.sort();
        return Ok(Merge {
            merged: incoming.to_string(),
            overwritten: displaced,
            verbatim: true,
        });
    }

    let mut overwritten = Vec::new();
    // Changed or added in the pushed file.
    for (key, value) in &inc {
        if base.get(key) == Some(value) {
            continue;
        }
        // Only a *disagreement* is an overwrite. Comparing the stored value
        // against the baseline alone reported one whenever the browser had
        // moved a field at all — including after a `pull`, where the file now
        // carries exactly what the browser wrote and nothing is displaced.
        // That made the workflow the docs recommend announce data loss that
        // had not happened, in the CLI and to the model both.
        if eff.get(key) != base.get(key) && eff.get(key) != Some(value) {
            overwritten.push(key.clone());
        }
        eff.insert(key.clone(), value.clone());
    }
    // Removed from the pushed file. A deletion is an edit like any other, or
    // a field could never be taken out of a file once the browser had touched
    // the record.
    for key in base.keys() {
        if inc.contains_key(key) {
            continue;
        }
        // Same rule: if the browser deleted it too, the file displaced
        // nothing.
        if eff.get(key) != base.get(key) && eff.contains_key(key) {
            overwritten.push(key.clone());
        }
        eff.remove(key);
    }
    overwritten.sort();

    // The merge landed exactly on the file: every browser edit was either
    // overwritten or was a field the file also carries unchanged. Store the
    // file as written rather than a re-serialisation of the same values.
    //
    // Two things depend on this. Comments and ordering survive whenever the
    // result is the file, not only when there was nothing to merge. And
    // `drifted` is a text comparison — without this, a push that won every
    // conflict would leave `baseline` and `effective` differing only in
    // formatting, so the cockpit would report unpulled edits that do not
    // exist and every later push would take the merge path for nothing.
    if eff == inc {
        return Ok(Merge {
            merged: incoming.to_string(),
            overwritten,
            verbatim: true,
        });
    }

    let merged = toml::to_string_pretty(&toml::Value::Table(eff))
        .map_err(|e| ManifestError::invalid(format!("serialising the merged record: {e}")))?;
    Ok(Merge {
        merged,
        overwritten,
        verbatim: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_round_trips_through_toml() {
        let source = "enabled = true\ndisplay_name = \"Alice\"\ntagline = \"Neuroscience\"\n\
                      timezone = \"America/New_York\"\n\
                      [[link]]\nkind = \"github\"\nurl = \"https://github.com/alice\"\n";
        let profile = Profile::from_toml(source).unwrap();
        assert!(profile.enabled);
        assert_eq!(profile.links[0].kind, LinkKind::Github);
        let again = Profile::from_toml(&profile.to_toml().unwrap()).unwrap();
        assert_eq!(profile, again);
    }

    #[test]
    fn a_profile_defaults_to_off_and_empty() {
        let profile = Profile::from_toml("").unwrap();
        assert!(!profile.enabled, "a hangar must not appear by upgrading");
        assert!(profile.display_name.is_none());
    }

    /// The one that would be a live XSS if the allowlist were a blocklist.
    #[test]
    fn a_link_may_only_be_http_or_https() {
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html;base64,PHN2Zz4=",
            "vbscript:msgbox",
            "file:///etc/passwd",
            "/relative/path",
        ] {
            let source = format!("[[link]]\nurl = \"{bad}\"\n");
            assert!(
                Profile::from_toml(&source).is_err(),
                "`{bad}` was accepted as a link"
            );
        }
        assert!(Profile::from_toml("[[link]]\nurl = \"https://ok.example\"\n").is_ok());
    }

    /// `EST` and friends *do* parse — the IANA database still carries the
    /// legacy fixed-offset zones — which is exactly why parsing is not the
    /// test. A booking page rendered in `EST` is an hour wrong from March to
    /// November and looks right throughout.
    #[test]
    fn a_fixed_offset_zone_is_refused_even_though_it_parses() {
        assert!(
            "EST".parse::<chrono_tz::Tz>().is_ok(),
            "the premise of this test changed"
        );
        for bad in ["EST", "MST", "PST8PDT", "-05:00", "Eastern"] {
            assert!(
                Profile::from_toml(&format!("timezone = \"{bad}\"\n")).is_err(),
                "`{bad}` was accepted"
            );
        }
        for ok in ["America/New_York", "Europe/London", "UTC"] {
            assert!(
                Profile::from_toml(&format!("timezone = \"{ok}\"\n")).is_ok(),
                "`{ok}` was refused"
            );
        }
    }

    #[test]
    fn a_tagline_is_one_line() {
        assert!(Profile::from_toml("tagline = \"two\\nlines\"\n").is_err());
        assert!(Profile::from_toml("bio = \"two\\nlines\"\n").is_ok());
    }

    /// `theme` and `accent` were validated and stored and never rendered —
    /// a field that saves and is silently ignored is worse than one that is
    /// refused, because the author concludes the feature is broken rather
    /// than absent. They come back with the renderer that reads them.
    #[test]
    fn a_palette_is_refused_until_something_renders_it() {
        for absent in ["theme = \"paper\"\n", "accent = \"#5d5294\"\n"] {
            assert!(Profile::from_toml(absent).is_err(), "{absent} was accepted");
        }
        assert!(Board::from_toml("slug = \"x\"\ntheme = \"paper\"\n").is_err());
    }

    #[test]
    fn a_board_without_a_slug_is_the_hangar_and_declares_no_lines() {
        let hangar = Board::from_toml("heading = \"Alice\"\n").unwrap();
        assert_eq!(hangar.kind(), BoardKind::Hangar);
        let err =
            Board::from_toml("[[entry]]\nkind = \"form\"\nid = \"letter\"\nlabel = \"Write\"\n")
                .unwrap_err();
        assert!(
            err.to_string().contains("wired from what is public"),
            "{err}"
        );
    }

    #[test]
    fn a_reference_line_names_an_id_and_a_link_line_names_a_url() {
        let ok = Board::from_toml(
            "slug = \"hello\"\n\
             [[entry]]\nkind = \"booking\"\nid = \"office-hours\"\nlabel = \"Book\"\n\
             [[entry]]\nkind = \"link\"\nurl = \"https://lab.example\"\nlabel = \"Lab\"\n",
        )
        .unwrap();
        assert_eq!(ok.kind(), BoardKind::Switchboard);
        assert_eq!(ok.entries.len(), 2);

        // A reference carrying a URL is the case that matters: it would be a
        // line the server cannot resolve, wearing a kind that says it can.
        let err = Board::from_toml(
            "slug = \"hello\"\n\
             [[entry]]\nkind = \"form\"\nurl = \"https://elsewhere.example\"\nlabel = \"x\"\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("names an id, never a url"),
            "{err}"
        );

        for bad in [
            "[[entry]]\nkind = \"form\"\nlabel = \"x\"\n",
            "[[entry]]\nkind = \"link\"\nlabel = \"x\"\n",
            "[[entry]]\nkind = \"link\"\nid = \"a\"\nurl = \"https://a.example\"\nlabel = \"x\"\n",
        ] {
            assert!(
                Board::from_toml(&format!("slug = \"hello\"\n{bad}")).is_err(),
                "accepted: {bad}"
            );
        }
    }

    #[test]
    fn a_reserved_slug_is_refused() {
        for slug in ["v", "account", "view", "slides"] {
            let err = Board::from_toml(&format!("slug = \"{slug}\"\n")).unwrap_err();
            assert!(err.to_string().contains("reserved"), "{slug}: {err}");
        }
        assert!(Board::from_toml("slug = \"teaching\"\n").is_ok());
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_dropped() {
        // A typo'd key that parsed and did nothing would be a page silently
        // missing the thing its author wrote.
        assert!(Profile::from_toml("taglin = \"typo\"\n").is_err());
        assert!(Board::from_toml("slug = \"x\"\nheadng = \"typo\"\n").is_err());
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    const BASE: &str =
        "# my profile\nenabled = true\ntagline = \"Neuroscience\"\nlocation = \"Hanover\"\n";

    /// The overwhelmingly common case: nobody has opened the cockpit, so the
    /// file arrives and is stored as it was written. Comments survive.
    #[test]
    fn an_unedited_record_takes_the_push_verbatim() {
        let pushed = "# my profile\nenabled = true\ntagline = \"Computational neuroscience\"\n";
        let merge = merge_push(BASE, BASE, pushed).unwrap();
        assert!(merge.verbatim);
        assert_eq!(merge.merged, pushed, "comments and spacing must survive");
        assert!(merge.overwritten.is_empty());
    }

    /// The reason the merge exists: a tagline fixed on a phone must not be
    /// reverted by tonight's push of an unchanged file.
    #[test]
    fn a_browser_edit_survives_a_push_that_did_not_touch_it() {
        let edited = "enabled = true\ntagline = \"Fixed on my phone\"\nlocation = \"Hanover\"\n";
        let pushed =
            "# my profile\nenabled = true\ntagline = \"Neuroscience\"\nlocation = \"Lebanon\"\n";
        let merge = merge_push(BASE, edited, pushed).unwrap();
        assert!(!merge.verbatim);
        let out = Profile::from_toml(&merge.merged).unwrap();
        assert_eq!(out.tagline.as_deref(), Some("Fixed on my phone"));
        // The field the file *did* change still lands.
        assert_eq!(out.location.as_deref(), Some("Lebanon"));
        assert!(merge.overwritten.is_empty(), "nothing was in conflict");
    }

    /// Both sides changed the same field. The file wins, and says so.
    #[test]
    fn a_conflict_goes_to_the_file_and_is_named() {
        let edited = "enabled = true\ntagline = \"From the browser\"\nlocation = \"Hanover\"\n";
        let pushed = "enabled = true\ntagline = \"From the file\"\nlocation = \"Hanover\"\n";
        let merge = merge_push(BASE, edited, pushed).unwrap();
        assert_eq!(merge.overwritten, vec!["tagline".to_string()]);
        let out = Profile::from_toml(&merge.merged).unwrap();
        assert_eq!(out.tagline.as_deref(), Some("From the file"));
    }

    /// A field taken out of the file is taken out of the record. Otherwise a
    /// line could never be deleted once the browser had touched anything.
    #[test]
    fn a_field_removed_from_the_file_is_removed() {
        let edited = "enabled = true\ntagline = \"Neuroscience\"\nlocation = \"Hanover\"\nbio = \"Added here\"\n";
        let pushed = "enabled = true\ntagline = \"Neuroscience\"\n";
        let merge = merge_push(BASE, edited, pushed).unwrap();
        let out = Profile::from_toml(&merge.merged).unwrap();
        assert!(out.location.is_none(), "location was dropped from the file");
        assert_eq!(
            out.bio.as_deref(),
            Some("Added here"),
            "the browser's own field stays"
        );
    }

    /// Boards go through the same function — the merge knows nothing about
    /// which record it is folding, which is why there is one of it.
    #[test]
    fn a_board_merges_by_the_same_rules() {
        let base = "slug = \"hello\"\nheading = \"Get in touch\"\n";
        let edited = "slug = \"hello\"\nheading = \"Say hello\"\n";
        let pushed = "slug = \"hello\"\nheading = \"Get in touch\"\nintro = \"Pick one.\"\n";
        let merge = merge_push(base, edited, pushed).unwrap();
        let board = Board::from_toml(&merge.merged).unwrap();
        assert_eq!(board.heading.as_deref(), Some("Say hello"));
        assert_eq!(board.intro.as_deref(), Some("Pick one."));
        assert_eq!(board.kind(), BoardKind::Switchboard);
    }

    /// An array is one value, so a line added in the browser and a line added
    /// in the file do not silently interleave into an order neither of them
    /// wrote. The file wins the whole list and says it did.
    #[test]
    fn a_list_is_one_field_and_never_half_merged() {
        let base = "slug = \"hello\"\n";
        let edited = "slug = \"hello\"\n[[entry]]\nkind = \"link\"\nurl = \"https://a.example\"\nlabel = \"A\"\n";
        let pushed = "slug = \"hello\"\n[[entry]]\nkind = \"link\"\nurl = \"https://b.example\"\nlabel = \"B\"\n";
        let merge = merge_push(base, edited, pushed).unwrap();
        assert_eq!(merge.overwritten, vec!["entry".to_string()]);
        let board = Board::from_toml(&merge.merged).unwrap();
        assert_eq!(board.entries.len(), 1);
        assert_eq!(board.entries[0].label, "B");
    }
}

#[cfg(test)]
mod merge_settling_tests {
    use super::*;

    /// A push that wins every conflict lands *on* the file, so the file is
    /// what gets stored — comments and all — and the record is not left
    /// looking drifted by a formatting difference alone.
    #[test]
    fn a_push_that_wins_everything_settles_back_to_the_file() {
        let base = "# mine\ntagline = \"Neuroscience\"\n";
        let edited = "tagline = \"From the browser\"\n";
        let pushed = "# mine\ntagline = \"From the file\"\n";
        let merge = merge_push(base, edited, pushed).unwrap();
        assert_eq!(merge.overwritten, vec!["tagline".to_string()]);
        assert!(merge.verbatim, "the result is the file, so store the file");
        assert_eq!(merge.merged, pushed);
    }

    /// And when a browser edit genuinely survives, it is not verbatim — the
    /// record really is something neither text is.
    #[test]
    fn a_surviving_edit_leaves_the_record_off_the_file() {
        let base = "tagline = \"a\"\nlocation = \"x\"\n";
        let edited = "tagline = \"kept\"\nlocation = \"x\"\n";
        let pushed = "tagline = \"a\"\nlocation = \"y\"\n";
        let merge = merge_push(base, edited, pushed).unwrap();
        assert!(!merge.verbatim);
        let out = Profile::from_toml(&merge.merged).unwrap();
        assert_eq!(out.tagline.as_deref(), Some("kept"));
        assert_eq!(out.location.as_deref(), Some("y"));
    }
}

#[cfg(test)]
mod merge_review_tests {
    use super::*;

    /// A record written in the cockpit has no baseline, so the first push has
    /// nothing to reconcile against and takes the file whole — naming what it
    /// displaced, which is what makes `pull` the obvious first move.
    #[test]
    fn a_first_push_over_a_cockpit_record_replaces_and_names_what_it_displaced() {
        let browser = "enabled = true\nbio = \"written here\"\ndisplay_name = \"Alice\"\n";
        let file = "enabled = true\ndisplay_name = \"Alice\"\n";
        let merge = merge_push("", browser, file).unwrap();
        assert_eq!(merge.merged, file, "the file becomes the record");
        assert_eq!(merge.overwritten, vec!["bio".to_string()]);
        // And it is now removable, which was the whole complaint: the second
        // push has a baseline and the ordinary deletion path applies.
        let merge = merge_push(file, file, "enabled = true\n").unwrap();
        let profile = Profile::from_toml(&merge.merged).unwrap();
        assert!(profile.display_name.is_none(), "a deletion still applies");
    }

    /// An empty record is not a displacement, so a first push onto nothing is
    /// silent rather than announcing it overwrote a page nobody wrote.
    #[test]
    fn a_first_push_onto_an_empty_record_displaces_nothing() {
        let merge = merge_push("", "", "enabled = true\n").unwrap();
        assert!(merge.overwritten.is_empty(), "{:?}", merge.overwritten);
    }

    /// The workflow the docs recommend must not announce data loss that did
    /// not happen: after a pull the file carries exactly what the browser
    /// wrote, so the push displaces nothing.
    #[test]
    fn pull_then_push_reports_no_overwrite() {
        let base = "tagline = \"a\"\nlocation = \"x\"\n";
        let edited = "tagline = \"kept\"\nlocation = \"x\"\n";
        // What `pull` writes to the file is the stored record.
        let merge = merge_push(base, edited, edited).unwrap();
        assert!(
            merge.overwritten.is_empty(),
            "a pull-then-push announced a loss that did not happen: {:?}",
            merge.overwritten
        );
        let profile = Profile::from_toml(&merge.merged).unwrap();
        assert_eq!(profile.tagline.as_deref(), Some("kept"));
    }

    /// Both sides deleting the same field is agreement, not a conflict.
    #[test]
    fn a_deletion_both_sides_made_is_not_an_overwrite() {
        let base = "tagline = \"a\"\nlocation = \"x\"\n";
        let edited = "location = \"x\"\n";
        let file = "location = \"y\"\n";
        let merge = merge_push(base, edited, file).unwrap();
        assert!(merge.overwritten.is_empty(), "{:?}", merge.overwritten);
        let profile = Profile::from_toml(&merge.merged).unwrap();
        assert!(profile.tagline.is_none());
        assert_eq!(profile.location.as_deref(), Some("y"));
    }

    /// A genuine disagreement still reports, or the warning would be useless.
    #[test]
    fn a_real_conflict_is_still_named() {
        let base = "tagline = \"a\"\n";
        let merge = merge_push(base, "tagline = \"browser\"\n", "tagline = \"file\"\n").unwrap();
        assert_eq!(merge.overwritten, vec!["tagline".to_string()]);
    }
}
