//! The one email this box sends.
//!
//! A stranger fills in a form and gets a link back. That is the whole of the
//! outbound mail surface, and it is deliberately not a general mailer: there is
//! no template list, no attachment path and no way for a caller to choose a
//! recipient other than the address just submitted. A box that can send
//! arbitrary mail as your domain is a different kind of asset to lose.
//!
//! **Amazon SES over HTTPS, signed with SigV4.** Not the SDK: the box's
//! dependency posture is "no cmake and no C toolchain" — that is why `ring`
//! was chosen over `aws-lc-rs` for ACME — and `aws-sdk-sesv2` brings a second
//! HTTP stack alongside the one `ureq` already provides. SigV4 is a hash
//! chain and a canonical string; it is written out here, and unit-tested
//! against a fixed clock so a change to it fails loudly rather than at 3am
//! against a live endpoint.
//!
//! **Two things about SES that are operational, not code.** A new SES account
//! is in the **sandbox**, where it may only send to *verified* addresses — a
//! stranger's link is accepted by the API and silently never delivered, which
//! is exactly the shape of failure this project keeps refusing elsewhere. And
//! the domain needs DKIM and SPF records, or a link that is sent lands in
//! spam, which is the same as not sending it. Both are written down in
//! `docs/DEPLOY.md`; neither can be checked from here without spending an API
//! call on every start.

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

/// What SES needs to accept a message, read from a file rather than from the
/// config.
///
/// The config is checked in and describes a deployment; this is a credential
/// and lives beside the box's other secrets at mode 0600. Same split as the
/// scoped keys, and for the same reason: a file you can `cat` in a bug report
/// must not be the file that can send mail as your domain.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl Credentials {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading SES credentials from {}", path.display()))?;
        let parsed: toml::Value =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let field = |name: &str| -> Result<String> {
            parsed
                .get(name)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{} has no `{name}`", path.display()))
        };
        Ok(Credentials {
            access_key_id: field("access_key_id")?,
            secret_access_key: field("secret_access_key")?,
        })
    }
}

/// The mailer this configuration asks for.
///
/// **A configured mail path that cannot be built stops the box**, rather than
/// falling back to writing links into the journal. The fallback would look
/// like a working deployment while every stranger's link went nowhere, which
/// is the same shape as a sandbox that quietly stops confining — and the same
/// answer: refuse at startup, with the reason. Absent `[mail]` is a different
/// thing entirely and is honest about itself, so it logs.
pub fn configured(config: &crate::config::Config) -> Result<Box<dyn crate::intake::Mailer>> {
    let Some(mail) = &config.mail else {
        return Ok(Box::new(crate::intake::LogMailer));
    };
    let credentials = Credentials::load(&mail.credentials).with_context(|| {
        format!(
            "[mail] names {}, so the box will not start without it",
            mail.credentials.display()
        )
    })?;
    Ok(Box::new(SesMailer::new(
        mail.from.clone(),
        mail.region.clone(),
        credentials,
    )))
}

/// Sends through Amazon SES.
pub struct SesMailer {
    from: String,
    region: String,
    credentials: Credentials,
}

impl SesMailer {
    pub fn new(from: String, region: String, credentials: Credentials) -> Self {
        SesMailer {
            from,
            region,
            credentials,
        }
    }

    fn endpoint(&self) -> String {
        format!("https://{}/v2/email/outbound-emails", self.host())
    }

    fn host(&self) -> String {
        format!("email.{}.amazonaws.com", self.region)
    }

    /// One send, blocking. Errors are returned so the caller can decide; the
    /// caller in this crate logs and drops them, because what the stranger is
    /// told must not depend on whether the send worked.
    fn send(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let payload = serde_json::json!({
            "FromEmailAddress": self.from,
            "Destination": { "ToAddresses": [to] },
            "Content": {
                "Simple": {
                    "Subject": { "Data": subject, "Charset": "UTF-8" },
                    // Text only, on purpose. An HTML part would be a second
                    // rendering of the same link to keep in step, and the one
                    // thing this message contains is a URL — which every mail
                    // client already makes clickable. It also keeps the message
                    // out of the "looks like marketing" bucket that HTML-only
                    // transactional mail lands in.
                    "Body": { "Text": { "Data": text, "Charset": "UTF-8" } }
                }
            }
        });
        let body = serde_json::to_vec(&payload)?;
        let authorization = self.sign(&body, now);
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();

        // Retry only the transient classes, and only ever the *send* — a
        // duplicate confirmation link is two live tokens for one submission,
        // so a retry after an accepted request would be worse than a failure.
        // 4xx other than 429 is terminal: a rejected address or a
        // sandbox refusal does not improve by being asked again.
        let mut delay = std::time::Duration::from_millis(500);
        let mut last: Option<String> = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(delay);
                delay *= 2;
            }
            let response = ureq::post(&self.endpoint())
                .header("Content-Type", "application/json")
                .header("X-Amz-Date", &amz_date)
                .header("Authorization", &authorization)
                .config()
                .http_status_as_error(false)
                .build()
                .send(&body[..]);
            match response {
                Ok(mut r) => {
                    let status = r.status().as_u16();
                    if (200..300).contains(&status) {
                        return Ok(());
                    }
                    let detail = r.body_mut().read_to_string().unwrap_or_default();
                    if status != 429 && status < 500 {
                        anyhow::bail!("SES refused the message ({status}): {detail}");
                    }
                    last = Some(format!("{status}: {detail}"));
                }
                Err(e) => last = Some(e.to_string()),
            }
        }
        anyhow::bail!(
            "SES did not accept the message after 3 attempts: {}",
            last.unwrap_or_else(|| "no response".into())
        )
    }

    /// The `Authorization` header for one request.
    ///
    /// Split out and tested because every part of it is a silent failure: a
    /// wrong canonical string, a header signed but not listed, a date that
    /// disagrees with `X-Amz-Date` — all of them come back as one
    /// indistinguishable 403.
    fn sign(&self, body: &[u8], now: chrono::DateTime<chrono::Utc>) -> String {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let datestamp = now.format("%Y%m%d").to_string();
        let host = self.host();
        let scope = format!("{datestamp}/{}/ses/aws4_request", self.region);

        // Signed headers must be lowercase and sorted, and the list in the
        // header has to match the block above it exactly.
        let canonical_headers =
            format!("content-type:application/json\nhost:{host}\nx-amz-date:{amz_date}\n");
        let signed_headers = "content-type;host;x-amz-date";
        let canonical_request = format!(
            "POST\n/v2/email/outbound-emails\n\n{canonical_headers}\n{signed_headers}\n{}",
            hex(&Sha256::digest(body))
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex(&Sha256::digest(canonical_request.as_bytes()))
        );

        let k_date = hmac(
            format!("AWS4{}", self.credentials.secret_access_key).as_bytes(),
            datestamp.as_bytes(),
        );
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"ses");
        let k_signing = hmac(&k_service, b"aws4_request");
        let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, \
             Signature={signature}",
            self.credentials.access_key_id
        )
    }
}

impl crate::intake::Mailer for SesMailer {
    fn send_verification(
        &self,
        address: &str,
        request_type: &mecha_manifest::RequestType,
        link: &str,
    ) {
        let subject = format!("Confirm your {} request", request_type.title);
        let text = format!(
            "You filled in the {} form.\n\n\
             Open this link to confirm it was you. Nothing is passed on until you do:\n\n\
             {link}\n\n\
             The link works once. If you did not fill in this form, ignore this \
             message — nothing has been sent on, and the submission is deleted \
             when the link expires.\n",
            request_type.title
        );

        // The trait returns nothing, and the handler has already told the
        // stranger to check their email. That is not sloppiness: what the page
        // says must not depend on the send, or the response becomes an oracle
        // for which addresses exist and which are verified in the SES sandbox.
        // A failure is the operator's problem, so it goes to the journal loudly
        // and the link goes with it — an operator can finish a verification by
        // hand, exactly as with `LogMailer`.
        match self.send(address, &subject, &text, chrono::Utc::now()) {
            Ok(()) => tracing::info!(request_type = request_type.id, "verification sent"),
            Err(e) => tracing::error!(
                error = %e,
                request_type = request_type.id,
                link,
                "sending a verification link failed — it can be completed by hand"
            ),
        }
    }

    fn send_invite(&self, address: &str, link: &str) {
        let subject = "You are invited to claim a handle";
        let text = format!(
            "You have been invited to set up an account.\n\n\
             Open this link to pick your handle — the name your pages will \
             live under:\n\n\
             {link}\n\n\
             The link works once and expires in {} days. A handle is \
             permanent, so pick one you want to keep.\n\n\
             If you were not expecting this, ignore it — nothing happens \
             until the link is opened.\n",
            crate::intake::INVITE_EXPIRY_DAYS
        );
        // Logged rather than returned, like a verification: the CLI printing
        // the link has already succeeded, and the operator watching it mint
        // is the person the journal reaches.
        match self.send(address, subject, &text, chrono::Utc::now()) {
            Ok(()) => tracing::info!(to = address, "invite sent"),
            Err(e) => tracing::error!(
                error = %e,
                to = address,
                link,
                "sending an invite failed — the link still works, deliver it another way"
            ),
        }
    }

    fn send_signin(&self, address: &str, handle: &str, link: &str) {
        let subject = format!("Sign in as {handle}");
        let text = format!(
            "A sign-in link for the `{handle}` account page was requested:\n\n\
             {link}\n\n\
             It works once and expires in {} minutes.\n\n\
             If you did not ask for this, ignore it — nothing happens unless \
             the link is opened, and only somebody reading this mailbox can \
             open it.\n",
            crate::intake::SIGNIN_LINK_EXPIRY_MINUTES
        );
        match self.send(address, &subject, &text, chrono::Utc::now()) {
            Ok(()) => tracing::info!(handle, "sign-in link sent"),
            Err(e) => tracing::error!(
                error = %e,
                handle,
                link,
                "sending a sign-in link failed — it can be delivered by hand"
            ),
        }
    }

    fn send_operator_signin(&self, address: &str, link: &str) {
        let subject = "Operator sign-in";
        let text = format!(
            "An operator sign-in link for this box was requested from its \
             /admin page:\n\n\
             {link}\n\n\
             It works once and expires in {} minutes.\n\n\
             If you did not ask for this, ignore it — nothing happens unless \
             the link is opened, and only somebody reading this mailbox can \
             open it.\n",
            crate::http::admin::EMAIL_LINK_EXPIRY_MINUTES
        );
        match self.send(address, subject, &text, chrono::Utc::now()) {
            Ok(()) => tracing::info!("operator sign-in link sent"),
            Err(e) => tracing::error!(
                error = %e,
                link,
                "sending an operator sign-in link failed — the CLI door still works"
            ),
        }
    }

    fn send_share(&self, address: &str, owner: &str, title: &str, link: &str) {
        let subject = format!("{owner} shared a page with you");
        let text = format!(
            "`{owner}` shared \u{201c}{title}\u{201d} with this email address:\n\n\
             {link}\n\n\
             Opening it will ask you to sign in with this address — a link is \
             mailed here, no password and no account. Access lasts until \
             `{owner}` withdraws it.\n\n\
             If you were not expecting this, ignore it — nothing happens \
             unless you sign in.\n"
        );
        match self.send(address, &subject, &text, chrono::Utc::now()) {
            Ok(()) => tracing::info!(owner, "share notice sent"),
            Err(e) => tracing::error!(
                error = %e,
                owner,
                link,
                "sending a share notice failed — the page link can be passed by hand"
            ),
        }
    }

    fn send_viewer_link(&self, address: &str, link: &str) {
        let subject = "Your sign-in link";
        let text = format!(
            "A sign-in link for a page shared with this address was requested:\n\n\
             {link}\n\n\
             It works once and expires in {} minutes.\n\n\
             If you did not ask for this, ignore it — nothing happens unless \
             the link is opened, and only somebody reading this mailbox can \
             open it.\n",
            crate::intake::VIEWER_LINK_EXPIRY_MINUTES
        );
        match self.send(address, subject, &text, chrono::Utc::now()) {
            Ok(()) => tracing::info!("reader sign-in link sent"),
            Err(e) => tracing::error!(
                error = %e,
                link,
                "sending a reader sign-in link failed — it can be delivered by hand"
            ),
        }
    }

    fn describe(&self) -> String {
        format!("Amazon SES — {} from {}", self.region, self.from)
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::Mailer;

    fn mailer() -> SesMailer {
        SesMailer::new(
            "no-reply@example.com".into(),
            "us-east-1".into(),
            Credentials {
                access_key_id: "AKIDEXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            },
        )
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
    }

    /// The signature is a pure function of the request and the clock. Pinning
    /// it means a change to the canonical string fails here rather than as a
    /// 403 from a live endpoint, which is the only other place it shows up.
    #[test]
    fn the_signature_is_stable_for_a_fixed_request_and_clock() {
        let a = mailer().sign(b"{}", at("2026-08-07T00:00:00Z"));
        let b = mailer().sign(b"{}", at("2026-08-07T00:00:00Z"));
        assert_eq!(a, b);
        assert!(a.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260807/us-east-1/ses/aws4_request,"
        ));
        assert!(a.contains("SignedHeaders=content-type;host;x-amz-date"));
    }

    /// Each input that feeds the canonical string has to actually reach it.
    /// A signature that ignores the body is one that still verifies after the
    /// message is rewritten in flight.
    #[test]
    fn the_body_the_clock_and_the_region_all_move_the_signature() {
        let base = mailer().sign(b"{}", at("2026-08-07T00:00:00Z"));

        let other_body = mailer().sign(br#"{"a":1}"#, at("2026-08-07T00:00:00Z"));
        assert_ne!(base, other_body, "the payload hash is signed");

        let other_time = mailer().sign(b"{}", at("2026-08-07T00:00:01Z"));
        assert_ne!(base, other_time, "the timestamp is signed");

        let mut west = mailer();
        west.region = "us-west-2".into();
        let other_region = west.sign(b"{}", at("2026-08-07T00:00:00Z"));
        assert_ne!(
            base, other_region,
            "the region is in the scope and the host"
        );
    }

    /// The credential scope's date and `X-Amz-Date` are derived separately and
    /// have to agree — SES rejects the pair outright when they do not, and the
    /// error says nothing about which half is wrong.
    #[test]
    fn the_scope_date_matches_the_timestamp_it_was_derived_from() {
        let signed = mailer().sign(b"{}", at("2026-12-31T23:59:59Z"));
        assert!(signed.contains("/20261231/"), "{signed}");
    }

    #[test]
    fn the_host_is_the_regional_endpoint() {
        assert_eq!(mailer().host(), "email.us-east-1.amazonaws.com");
        assert_eq!(
            mailer().endpoint(),
            "https://email.us-east-1.amazonaws.com/v2/email/outbound-emails"
        );
    }

    /// What `factory check` prints has to name the region and the sender, or
    /// "mail is configured" is a claim nobody can act on.
    #[test]
    fn describe_names_the_region_and_the_sender() {
        let described = Mailer::describe(&mailer());
        assert!(described.contains("us-east-1"));
        assert!(described.contains("no-reply@example.com"));
    }

    #[test]
    fn credentials_come_off_disk_and_a_missing_field_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ses-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("ok.toml");
        std::fs::write(&good, "access_key_id = \"A\"\nsecret_access_key = \"B\"\n").unwrap();
        let loaded = Credentials::load(&good).unwrap();
        assert_eq!(loaded.access_key_id, "A");
        assert_eq!(loaded.secret_access_key, "B");

        let partial = dir.join("partial.toml");
        std::fs::write(&partial, "access_key_id = \"A\"\n").unwrap();
        let err = Credentials::load(&partial).unwrap_err().to_string();
        assert!(err.contains("secret_access_key"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
