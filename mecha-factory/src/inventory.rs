//! What one user has, in one place — and whether a stranger can reach it.
//!
//! Four kinds of thing live under a user, in four tables that know nothing
//! about each other: **bundles** (joined to their alias), **forms** and
//! **booking pages** (both rows in `types`, told apart by the kind their
//! manifest declares), and **polls**. Before this module the account page
//! answered "what have I got" with `bundles_overview` alone, so the owner's
//! own view of their surface was missing three quarters of it.
//!
//! **One inventory, several renderings.** The account page shows everything
//! with its controls; a public index shows what is public; a curated page
//! picks from the same set. They differ in a *filter* and a *chrome* and
//! never in a source, which is the rule `http/booking.rs`'s `open_slots`
//! already states for availability: two copies of a subtraction is how two
//! surfaces end up disagreeing about what is open. The failure mode here has
//! the same shape and a worse ending — a public page advertising a form its
//! owner deleted, or naming a bundle nobody may read.
//!
//! **[`Reach`] is the whole point of the module.** Every kind reaches
//! "can a stranger open this?" by a different route:
//!
//! - a bundle needs a public alias, a version behind it, and that version not
//!   withheld;
//! - a form or a booking page needs a manifest that parses and declares
//!   `[verification]` — [`RequestType::servable`];
//! - a poll needs a link audience and an open state, because a roster poll's
//!   every real URL contains somebody's ballot capability. There is no URL to
//!   advertise, so listing one is not useless — it is the shape of an
//!   accident.
//!
//! Each of those is computed here, once, by the same reasoning the handler
//! that serves the bytes uses. A second opinion about who may read what is
//! how a private artifact ends up named on a public page, where the title
//! alone is the leak.

use mecha_manifest::{AudienceKind, RequestKind, RequestType, Visibility};

use crate::db::{Db, UserRow};

/// Whether a stranger can reach a thing right now, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// Open to anyone, at this gate-relative path.
    Public(String),
    /// Not reachable by a stranger. The reason is for the owner's eyes — it
    /// is the answer to "why is this not on my page", which is a question
    /// somebody will otherwise ask us.
    Closed(&'static str),
}

impl Reach {
    pub fn is_public(&self) -> bool {
        matches!(self, Reach::Public(_))
    }

    /// The path, when there is one. What a public index links to.
    pub fn path(&self) -> Option<&str> {
        match self {
            Reach::Public(path) => Some(path),
            Reach::Closed(_) => None,
        }
    }

    /// The owner-facing explanation, when it is closed.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Reach::Public(_) => None,
            Reach::Closed(why) => Some(*why),
        }
    }
}

/// One published bundle, with the release state its owner acts on.
#[derive(Debug, Clone)]
pub struct BundleItem {
    pub id: String,
    pub title: String,
    /// The newest version stored, whether or not anything points at it.
    pub latest: u32,
    /// What the share URL resolves to, if anything.
    pub aliased: Option<u32>,
    pub visibility: Visibility,
    /// Whether the aliased version has been withheld by an operator.
    pub withheld: bool,
    pub reach: Reach,
}

/// One form or booking page. Both are rows in `types`; `kind` is what the
/// stored manifest declares, and it decides which of the two routes serves
/// it.
#[derive(Debug, Clone)]
pub struct TypeItem {
    pub id: String,
    pub title: String,
    pub kind: RequestKind,
    pub reach: Reach,
}

/// One poll.
#[derive(Debug, Clone)]
pub struct PollItem {
    pub id: String,
    pub title: String,
    pub open: bool,
    /// A link audience answers at one shared URL; a roster audience answers
    /// at one capability URL per participant. Only the first has a URL that
    /// can be published.
    pub link_audience: bool,
    pub reach: Reach,
}

/// Everything one user has.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub bundles: Vec<BundleItem>,
    pub forms: Vec<TypeItem>,
    pub bookings: Vec<TypeItem>,
    pub polls: Vec<PollItem>,
}

impl Inventory {
    /// Read it. A failing query yields an empty list for that kind rather
    /// than failing the page: the account page is where somebody goes when
    /// something is wrong, and it is worth still rendering three sections
    /// when one is unreadable.
    pub fn read(db: &Db, user: &UserRow) -> Self {
        let mut inv = Inventory::default();
        let now = chrono::Utc::now();

        for row in db.bundles_overview(&user.id).unwrap_or_default() {
            // The same three conditions `http/artifacts.rs` reaches before it
            // will serve a byte, in the same order, so that "listed" can
            // never mean more than "servable".
            let reach = match (row.visibility, row.aliased, row.withheld) {
                (_, _, true) => Reach::Closed("the live version is withheld"),
                (Visibility::Private, _, _) => Reach::Closed("private"),
                (Visibility::Public, None, _) => Reach::Closed("taken down"),
                (Visibility::Public, Some(_), false) => {
                    // The two-segment viewer URL, which follows the alias.
                    // Pinning a version here would make a link go stale in
                    // the one way nobody notices — silently, still working,
                    // showing last month.
                    Reach::Public(format!("/view/{}/{}", user.handle, row.id))
                }
            };
            inv.bundles.push(BundleItem {
                id: row.id,
                title: row.title,
                latest: row.latest,
                aliased: row.aliased,
                visibility: row.visibility,
                withheld: row.withheld,
                reach,
            });
        }

        for row in db.types(&user.id).unwrap_or_default() {
            // An unparseable manifest is closed rather than skipped. It is
            // still the user's instrument and it still needs to appear on
            // their own page saying what is wrong with it; dropping the row
            // would make a broken form indistinguishable from a deleted one.
            let (kind, reach) = match RequestType::from_toml(&row.manifest) {
                Err(_) => (
                    RequestKind::Request,
                    Reach::Closed("the manifest no longer parses"),
                ),
                Ok(parsed) => {
                    let kind = parsed.kind;
                    let reach = if parsed.servable().is_err() {
                        Reach::Closed("declares no [verification], so it is not served")
                    } else {
                        let prefix = match kind {
                            RequestKind::Request => "f",
                            RequestKind::Booking => "s",
                        };
                        Reach::Public(format!("/{prefix}/{}/{}", user.handle, row.id))
                    };
                    (kind, reach)
                }
            };
            let item = TypeItem {
                id: row.id,
                title: row.title,
                kind,
                reach,
            };
            match kind {
                RequestKind::Request => inv.forms.push(item),
                RequestKind::Booking => inv.bookings.push(item),
            }
        }

        for row in db.polls(&user.id).unwrap_or_default() {
            // The same predicate the handler at `/p/{handle}/{id}` applies.
            // Reading `state` alone was broader than the code it claims to
            // mirror: nothing closes a poll when its deadline passes, so an
            // expired one was advertised, rendered its ballot, and then
            // refused the submission.
            let open = crate::http::poll::still_open(&row, now);
            // A spec that no longer parses is not a link audience: fail
            // closed, exactly as `PollRow::general_spec` does for the page
            // itself, rather than guessing at an audience and publishing a
            // URL that answers nobody.
            let link_audience = matches!(
                row.general_spec(),
                Ok(Some(spec)) if spec.audience.kind == AudienceKind::Link
            );
            let reach = match (link_audience, open) {
                (false, _) => Reach::Closed(
                    "a roster poll has one capability URL per participant, and no shared one",
                ),
                (true, false) => Reach::Closed("closed"),
                (true, true) => Reach::Public(format!("/p/{}/{}", user.handle, row.id)),
            };
            inv.polls.push(PollItem {
                id: row.id,
                title: row.title,
                open,
                link_audience,
                reach,
            });
        }

        inv
    }

    /// Nothing at all, of any kind.
    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty()
            && self.forms.is_empty()
            && self.bookings.is_empty()
            && self.polls.is_empty()
    }

    /// How many things a stranger could open. What a public index would have
    /// to show, and therefore the honest answer to "is my page worth turning
    /// on".
    pub fn public_count(&self) -> usize {
        self.bundles.iter().filter(|b| b.reach.is_public()).count()
            + self.forms.iter().filter(|t| t.reach.is_public()).count()
            + self.bookings.iter().filter(|t| t.reach.is_public()).count()
            + self.polls.iter().filter(|p| p.reach.is_public()).count()
    }
}

// ---- resolving a board's declared lines ---------------------------------

/// What became of one declared line.
///
/// A switchboard names artifacts by `kind` + `id`, and the server resolves
/// them — which is the whole reason an entry is a reference rather than a
/// URL. Resolution has exactly two outcomes, and both matter: a **lit** line
/// is rendered, and a **dark** one is omitted from the page and reported to
/// its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Lit {
        /// Gate-relative for a reference; absolute for an external link.
        href: String,
        label: String,
        blurb: Option<String>,
        /// The host, for a line that leaves our origin. Shown always: a page
        /// that made an off-origin link indistinguishable from a first-party
        /// one would be a phishing kit with a nice theme.
        external_host: Option<String>,
    },
    Dark {
        label: String,
        /// For the owner's eyes only. A stranger never learns that a line
        /// was here — the omission is the whole point.
        why: String,
    },
}

impl Line {
    pub fn is_lit(&self) -> bool {
        matches!(self, Line::Lit { .. })
    }
}

impl Inventory {
    /// Resolve one declared line against what actually exists.
    ///
    /// **A dark line is omitted, never rendered dead.** A button in the page
    /// somebody put in their email signature that answers 404 is worse than
    /// an absent one, because the person who clicks it concludes something
    /// about its owner rather than about the software.
    ///
    /// The two ways a line goes dark are kept apart in the message, because
    /// they need different fixes: the artifact is *gone* (delete the line) or
    /// it is *there and not public* (release it).
    pub fn resolve(&self, entry: &mecha_manifest::Entry) -> Line {
        use mecha_manifest::EntryKind;

        let label = entry.label.clone();
        let dark = |why: String| Line::Dark {
            label: label.clone(),
            why,
        };

        if entry.kind == EntryKind::Link {
            let Some(url) = entry.url.as_deref() else {
                return dark("a link line with no url".into());
            };
            return Line::Lit {
                href: url.to_string(),
                label,
                blurb: entry.blurb.clone(),
                external_host: Some(host_of(url)),
            };
        }

        let Some(id) = entry.id.as_deref() else {
            return dark(format!("a {:?} line with no id", entry.kind));
        };
        // One lookup shape for all four kinds: find it by id, then ask the
        // same `Reach` every other surface asks.
        let found: Option<(&Reach, &str)> = match entry.kind {
            EntryKind::Booking => self
                .bookings
                .iter()
                .find(|t| t.id == id)
                .map(|t| (&t.reach, "booking page")),
            EntryKind::Form => self
                .forms
                .iter()
                .find(|t| t.id == id)
                .map(|t| (&t.reach, "form")),
            EntryKind::Bundle => self
                .bundles
                .iter()
                .find(|b| b.id == id)
                .map(|b| (&b.reach, "bundle")),
            EntryKind::Poll => self
                .polls
                .iter()
                .find(|p| p.id == id)
                .map(|p| (&p.reach, "poll")),
            EntryKind::Link => unreachable!("handled above"),
        };

        match found {
            None => dark(format!("no {} called `{id}`", kind_word(entry.kind))),
            Some((Reach::Closed(why), what)) => {
                dark(format!("the {what} `{id}` is not public: {why}"))
            }
            Some((Reach::Public(path), _)) => Line::Lit {
                href: path.clone(),
                label,
                blurb: entry.blurb.clone(),
                external_host: None,
            },
        }
    }

    /// Every line of a board, in the order it was written.
    pub fn resolve_all(&self, board: &mecha_manifest::Board) -> Vec<Line> {
        board.entries.iter().map(|e| self.resolve(e)).collect()
    }
}

fn kind_word(kind: mecha_manifest::EntryKind) -> &'static str {
    use mecha_manifest::EntryKind;
    match kind {
        EntryKind::Booking => "booking page",
        EntryKind::Form => "form",
        EntryKind::Bundle => "bundle",
        EntryKind::Poll => "poll",
        EntryKind::Link => "link",
    }
}

/// The host out of a URL, for showing where an off-origin line goes.
///
/// **The userinfo is stripped, and that is the whole point of the function.**
/// `https://gate.example.org@evil.example/signin` is a legal URL that a
/// browser sends to `evil.example`; a reader shown everything before the
/// first `/` would read a host beginning with the name they trust. Splitting
/// on `://` and `/?#` alone made the host display an aid to the attack it
/// exists to prevent.
///
/// String work rather than a URL parser: the value already passed the
/// manifest's `http(s)`-only check, so what is left is display.
pub fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Everything up to the **last** `@` is userinfo. The last, not the first:
    // a password may itself contain one.
    match authority.rsplit_once('@') {
        Some((_, host)) => host.to_string(),
        None => authority.to_string(),
    }
}

#[cfg(test)]
mod host_tests {
    #[test]
    fn userinfo_never_reaches_the_reader() {
        for (url, host) in [
            ("https://evil.example/x", "evil.example"),
            (
                "https://gate.example.org@evil.example/signin",
                "evil.example",
            ),
            ("https://a@b@evil.example/", "evil.example"),
            (
                "https://user:pw@evil.example:8443/p?q#f",
                "evil.example:8443",
            ),
            ("https://plain.example", "plain.example"),
        ] {
            assert_eq!(super::host_of(url), host, "for {url}");
        }
    }
}
