//! `polls addin`: the sideloaded manifest for the PowerPoint content
//! add-in (SLIDES-RESEARCH.md §3).
//!
//! A content add-in is a webview whose HTTPS URL you control — effectively
//! a private Web Viewer, which matters because Microsoft killed its own.
//! The manifest below is the *entire* install artifact: ~60 lines of XML
//! pointing at the gate's `/slides/addin` wrapper, sideloaded once per
//! machine and never hosted anywhere.
//!
//! The GUID is **derived from the gate's URL**, not minted fresh per run:
//! Office treats the Id as the add-in's identity, so regenerating the
//! manifest must produce the same add-in (an upgrade), not a second one
//! accumulating in the sideload folder. Two gates get two ids, which is
//! also right — they are two add-ins pointing at two wrappers.

use sha2::{Digest, Sha256};

/// The manifest XML, ready to write to a file.
pub fn addin_manifest(gate: &str) -> String {
    let gate = gate.trim_end_matches('/');
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OfficeApp xmlns="http://schemas.microsoft.com/office/appforoffice/1.1"
           xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
           xsi:type="ContentApp">
  <Id>{id}</Id>
  <Version>1.0.0.0</Version>
  <ProviderName>mecha</ProviderName>
  <DefaultLocale>en-US</DefaultLocale>
  <DisplayName DefaultValue="Live poll"/>
  <Description DefaultValue="A mecha poll's live results, as an object on the slide."/>
  <Hosts>
    <Host Name="Presentation"/>
  </Hosts>
  <DefaultSettings>
    <SourceLocation DefaultValue="{gate}/slides/addin"/>
    <RequestedWidth>960</RequestedWidth>
    <RequestedHeight>540</RequestedHeight>
  </DefaultSettings>
  <Permissions>Restricted</Permissions>
</OfficeApp>
"#,
        id = deterministic_guid(gate),
    )
}

/// Where the manifest goes and how to load it, printed beside the file —
/// sideloading is the one step that happens outside our machinery, so the
/// instructions ride with the artifact instead of living in a wiki.
pub fn sideload_instructions(path: &str) -> String {
    format!(
        "Sideload it once per machine:\n\
         \n\
         PowerPoint for Mac:\n\
         cp {path} ~/Library/Containers/com.microsoft.Powerpoint/Data/Documents/wef/\n\
         then restart PowerPoint; Home → Add-ins → shows \"Live poll\".\n\
         \n\
         PowerPoint on the web: Home → Add-ins → More Settings →\n\
         Upload My Add-in → pick {path}.\n\
         (The web slideshow does not persist content add-ins; desktop is\n\
         the lecture surface.)\n\
         \n\
         Insert it on the slide you want the chart on, paste the poll's\n\
         projector URL when it asks, and the chart appears when the\n\
         slideshow starts. The browser window on the projector remains the\n\
         fallback that cannot break."
    )
}

/// A stable UUID from the gate URL: sha-256, sixteen bytes, the version
/// and variant bits set so it reads as a well-formed v4 UUID.
fn deterministic_guid(gate: &str) -> String {
    let digest = Sha256::digest(gate.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_points_at_the_gate_and_keeps_its_identity() {
        let manifest = addin_manifest("https://gate.example.edu/");
        assert!(
            manifest.contains("https://gate.example.edu/slides/addin"),
            "{manifest}"
        );
        assert!(manifest.contains("xsi:type=\"ContentApp\""));
        assert!(manifest.contains("<Host Name=\"Presentation\"/>"));
        // Least privilege: settings need no more than Restricted.
        assert!(manifest.contains("<Permissions>Restricted</Permissions>"));
        // Regenerating is an upgrade, not a second add-in — and the
        // trailing slash must not change who we are.
        assert_eq!(manifest, addin_manifest("https://gate.example.edu"));
    }

    #[test]
    fn the_guid_is_stable_well_formed_and_distinct_per_gate() {
        let a = deterministic_guid("https://a.example");
        assert_eq!(a, deterministic_guid("https://a.example"));
        assert_ne!(a, deterministic_guid("https://b.example"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version bits: {a}");
        assert!(
            matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "variant bits: {a}"
        );
    }
}
