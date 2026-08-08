//! Render a specimen poll page to a directory you can open in a browser —
//! the gallery's rule applied to the poll: a renderer whose output nobody
//! has looked at is a renderer with a bug in it.
//!
//! ```sh
//! cargo run --example poll-preview -- /tmp/poll
//! ```

use chrono::{DateTime, Duration, Utc};
use mecha_manifest::{poll_page, PollAnswer, PollCandidate, PollPageOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out: std::path::PathBuf = std::env::args()
        .nth(1)
        .ok_or("usage: poll-preview <output-dir>")?
        .into();
    std::fs::create_dir_all(&out)?;

    let base: DateTime<Utc> = "2026-08-18T17:00:00Z".parse()?;
    let candidate = |days: i64, hour: i64, mine, yes_count| PollCandidate {
        start: base + Duration::days(days) + Duration::hours(hour),
        end: base + Duration::days(days) + Duration::hours(hour + 1),
        duration_minutes: 60,
        mine,
        yes_count,
    };
    let candidates = vec![
        candidate(0, 0, Some(PollAnswer::Yes), 3),
        candidate(0, 2, Some(PollAnswer::No), 1),
        candidate(1, -4, Some(PollAnswer::IfNeeded), 2),
        candidate(1, 0, None, 4),
        candidate(3, 1, None, 0),
    ];
    let page = poll_page(
        &candidates,
        &PollPageOptions {
            title: "Lab meeting — week of Aug 17".into(),
            participant: "Priya".into(),
            timezone: "America/New_York".parse()?,
            action: String::new(),
            assets: String::new(),
            theme: mecha_manifest::theme::NOCTURNE,
            deadline_local: Some("Fri Aug 14, 5:00 PM EDT".into()),
            responded: 4,
            total: 6,
            open: true,
            notice: None,
        },
    );
    std::fs::write(out.join("index.html"), &page.html)?;
    for (name, contents) in mecha_manifest::booking_assets(&mecha_manifest::theme::NOCTURNE) {
        std::fs::write(out.join(name), contents)?;
    }
    println!("open {}", out.join("index.html").display());
    Ok(())
}
