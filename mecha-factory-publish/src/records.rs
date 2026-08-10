//! Which record of the personal public surface a command is talking about.
//!
//! Not to be confused with `main.rs`'s `mod surface`, which is the *tool*
//! surface -- which CLI commands an agent may reach. This one is about the
//! pages a stranger sees.
//!
//! Three things travel between this machine and the box — a profile, the
//! hangar's own heading, and each switchboard — and every verb that touches
//! one needs the same four answers: where it lives locally, what route it
//! goes to, what to call it in a sentence, and how to check it before the
//! network. Both front ends ask, so the answers live here rather than twice.
//!
//! The local layout is fixed rather than configurable, because a person and
//! an agent have to look in the same place — and because `pull` on a machine
//! that has never seen a board has to know where to put it.

use std::path::PathBuf;

use anyhow::Result;

/// One record, named.
pub enum Record {
    Profile,
    Hangar,
    Board(String),
}

/// Parse a tool or CLI argument into a record.
///
/// A `switchboard` with no slug is an error rather than a silent fallback to
/// the hangar: they are different pages with different audiences, and a model
/// that omitted the slug meant a board it could name.
pub fn record_of(what: &str, slug: &str) -> Result<Record> {
    match what {
        "profile" => Ok(Record::Profile),
        "hangar" => Ok(Record::Hangar),
        "switchboard" => {
            if slug.trim().is_empty() {
                anyhow::bail!("a switchboard is named by its slug — say which one");
            }
            Ok(Record::Board(slug.trim().to_string()))
        }
        other => anyhow::bail!("`{other}` is not a record: profile, hangar or switchboard"),
    }
}

impl Record {
    pub fn route(&self) -> String {
        match self {
            Record::Profile => "/v1/profile".into(),
            Record::Hangar => "/v1/hangar".into(),
            Record::Board(slug) => format!("/v1/boards/{slug}"),
        }
    }

    pub fn what(&self) -> String {
        match self {
            Record::Profile => "the profile".into(),
            Record::Hangar => "the hangar".into(),
            Record::Board(slug) => format!("switchboard `{slug}`"),
        }
    }

    pub fn default_path(&self) -> Result<PathBuf> {
        let dir = crate::remote::Remote::dir()?;
        Ok(match self {
            Record::Profile => dir.join("profile.toml"),
            Record::Hangar => dir.join("hangar.toml"),
            Record::Board(slug) => dir.join("switchboards").join(format!("{slug}.toml")),
        })
    }

    /// Validate before the network. The box checks too — it has to, since it
    /// cannot trust a client — but a person editing a file deserves the error
    /// without a round trip, and a model deserves it without spending a call.
    pub fn check(&self, text: &str) -> Result<()> {
        match self {
            Record::Profile => {
                mecha_manifest::Profile::from_toml(text)?;
            }
            _ => {
                mecha_manifest::Board::from_toml(text)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switchboard_without_a_slug_is_refused_not_defaulted() {
        assert!(record_of("switchboard", "").is_err());
        assert!(record_of("switchboard", "  ").is_err());
        assert!(record_of("switchboard", "teaching").is_ok());
    }

    #[test]
    fn the_routes_are_the_ones_the_box_serves() {
        assert_eq!(Record::Profile.route(), "/v1/profile");
        assert_eq!(Record::Hangar.route(), "/v1/hangar");
        assert_eq!(Record::Board("hello".into()).route(), "/v1/boards/hello");
    }

    #[test]
    fn a_profile_is_not_checked_as_a_board() {
        // `enabled` is a profile field and not a board one, so each type has
        // to be checked as itself or a typo lands on the box.
        assert!(Record::Profile.check("enabled = true\n").is_ok());
        assert!(Record::Hangar.check("enabled = true\n").is_err());
        assert!(Record::Board("x".into()).check("slug = \"x\"\n").is_ok());
    }
}
