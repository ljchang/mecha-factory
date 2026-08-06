//! Render a request type to a directory you can open in a browser.
//!
//! ```sh
//! cargo run --example render -- mecha-manifest/types/speaking.toml /tmp/out
//! xdg-open /tmp/out/index.html
//! ```
//!
//! The point of step 1 being "pure, unit-tested, renders to a file you can
//! open": a form generator whose output nobody has looked at in a browser is a
//! form generator with a bug in it. Everything a real publish needs beyond this
//! — versioning, the origin, the vendoring gate — comes later; what this proves
//! is that one manifest produces a page, a schema, and the assets they
//! reference, with no external requests in any of them.

use mecha_manifest::{FormOptions, RequestType};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let manifest: PathBuf = args
        .next()
        .ok_or("usage: render <manifest.toml> <output-dir>")?
        .into();
    let out: PathBuf = args
        .next()
        .ok_or("usage: render <manifest.toml> <output-dir>")?
        .into();

    let request_type = RequestType::from_toml(&std::fs::read_to_string(&manifest)?)?;
    std::fs::create_dir_all(&out)?;

    let page = request_type.form(&FormOptions {
        action: format!("/r/{}", request_type.id),
        ..FormOptions::default()
    });
    std::fs::write(out.join("index.html"), &page.html)?;
    for (name, contents) in page.assets() {
        std::fs::write(out.join(name), contents)?;
    }
    std::fs::write(
        out.join("schema.json"),
        serde_json::to_string_pretty(&request_type.json_schema())?,
    )?;

    println!(
        "{} v{} → {}",
        request_type.id,
        request_type.version,
        out.display()
    );
    println!(
        "  {} fields, {} of them free text, {} step(s)",
        request_type.fields.len(),
        request_type.free_text_fields().count(),
        request_type.steps.len().max(1)
    );
    println!("  open {}", out.join("index.html").display());
    Ok(())
}
