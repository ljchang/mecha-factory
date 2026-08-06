//! Verification tokens, and where a link actually goes.
//!
//! Two small things that decide whether a public form is an asset or a
//! liability.
//!
//! **The token is a bearer credential with a very short job.** It proves one
//! address consented to one submission, once. So it is random, it is stored as
//! a hash, and it is spent on use — the same three properties the publish key
//! has, for the same reason: reading the box's disk must not let anyone act.
//!
//! **Delivery is a sink, not a feature.** Verification means this box sends
//! mail to strangers on a user's behalf, from our domain: a spam cannon with a
//! form in front of it (§15.5). That is a deployment concern with a
//! reputation attached, so the state machine does not wait for it — it names an
//! interface, ships an implementation that writes the link to the log, and
//! **refuses to serve forms in production with no mailer configured**, because
//! a form that silently never sends a link is worse than one that will not
//! start.

use sha2::{Digest, Sha256};

/// Verification emails to one address, per user, per day.
///
/// Low on purpose and separate from the per-user budget: forty mails to one
/// person is abuse, and forty people once may be a conference.
pub const PER_RECIPIENT_PER_DAY: i64 = 3;

/// A fresh token, shown once in a link and never stored.
pub fn mint_token() -> String {
    crate::keys::random_id()
}

/// What the ledger holds instead of the token.
pub fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// A recipient, as something we can count without keeping a second copy of.
///
/// The address is already in the submission's payload; storing it again in an
/// indexed column would be a second place a subject-access request has to
/// reach. A salt-free hash is enough to count with and not enough to enumerate
/// from, given the alternative is the address in plain text.
pub fn recipient_hash(address: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(address.trim().to_ascii_lowercase().as_bytes())
    )
}

/// Where a verification link goes.
pub trait Mailer: Send + Sync {
    fn send_verification(
        &self,
        address: &str,
        request_type: &mecha_manifest::RequestType,
        link: &str,
    );
    /// What `factory check` prints.
    fn describe(&self) -> String;
}

/// Writes the link to the log instead of sending it.
///
/// For development and for a box that has not been given a mail path yet. It is
/// **not** silent: an operator reading the journal can complete a verification
/// by hand, which is what makes the whole flow testable before SMTP exists.
pub struct LogMailer;

impl Mailer for LogMailer {
    fn send_verification(
        &self,
        address: &str,
        request_type: &mecha_manifest::RequestType,
        link: &str,
    ) {
        tracing::info!(
            to = address,
            request_type = request_type.id,
            link,
            "verification link (not sent: no mailer configured)"
        );
    }

    fn describe(&self) -> String {
        "log — links are written to the journal and not sent".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored value is a verifier, not a link. Reading the ledger off a
    /// lost box must not let anyone confirm somebody else's submission.
    #[test]
    fn a_token_is_stored_as_a_hash_and_two_tokens_differ() {
        let a = mint_token();
        let b = mint_token();
        assert_ne!(a, b);
        assert_ne!(hash_token(&a), a);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    /// Counting sends per recipient must not depend on how they typed it.
    #[test]
    fn a_recipient_is_the_same_recipient_in_any_casing() {
        assert_eq!(
            recipient_hash("Ada@Example.ORG"),
            recipient_hash(" ada@example.org ")
        );
        assert_ne!(recipient_hash("ada@example.org"), recipient_hash("b@x.org"));
    }
}
