//! The personal public surface, as data: a profile, and the boards it wears.
//!
//! Two records travel between a person's machine and the box. A [`Profile`] is
//! who they are — a name, a line about themselves, some links, a theme. A
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
    /// One of the built-in themes by name. Unknown names are refused *here*
    /// so the author learns about a typo; the renderer still falls back,
    /// because a bad palette must never take a page down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// `#rgb` or `#rrggbb`, and nothing else. A CSS colour is a place a
    /// `url()` can hide, and no palette needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
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
    /// Overrides the profile's theme for this board alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
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
        check_theme(&self.theme)?;
        check_accent(&self.accent)?;
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
        check_theme(&self.theme)?;

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

fn check_theme(name: &Option<String>) -> Result<()> {
    let Some(name) = name else { return Ok(()) };
    if crate::BUILT_IN_THEMES
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(name))
    {
        return Ok(());
    }
    let known: Vec<&str> = crate::BUILT_IN_THEMES.iter().map(|t| t.name).collect();
    Err(ManifestError::invalid(format!(
        "`{name}` is not a theme — try one of: {}",
        known.join(", ")
    )))
}

/// `#rgb` or `#rrggbb`. Deliberately not "any CSS colour": a colour is a
/// place a `url()` can hide, and nothing about a palette needs one.
fn check_accent(accent: &Option<String>) -> Result<()> {
    let Some(value) = accent else { return Ok(()) };
    let hex = value.strip_prefix('#').unwrap_or("");
    if (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(ManifestError::invalid(format!(
        "`{value}` is not a colour — write `#5d5294` or `#abc`"
    )))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_round_trips_through_toml() {
        let source = "enabled = true\ndisplay_name = \"Alice\"\ntagline = \"Neuroscience\"\n\
                      timezone = \"America/New_York\"\ntheme = \"paper\"\naccent = \"#5d5294\"\n\
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

    #[test]
    fn an_accent_is_hex_and_never_a_function() {
        for bad in ["url(x)", "red", "#12345", "rgb(1,2,3)", "#gggggg"] {
            assert!(
                Profile::from_toml(&format!("accent = \"{bad}\"\n")).is_err(),
                "`{bad}` was accepted as an accent"
            );
        }
        assert!(Profile::from_toml("accent = \"#abc\"\n").is_ok());
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

    #[test]
    fn an_unknown_theme_is_named_rather_than_ignored() {
        let err = Profile::from_toml("theme = \"midnight\"\n").unwrap_err();
        assert!(err.to_string().contains("not a theme"), "{err}");
        assert!(Profile::from_toml("theme = \"Paper\"\n").is_ok(), "by case");
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
