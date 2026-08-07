//! What a bundle's files are served as.
//!
//! Here, beside the CSP table, for the same reason that table is here: the
//! preview server at home and the public server must answer identically, or
//! "verified locally" means verified against something the world never sees.
//! A media type is not decoration — three of the entries below decide whether a
//! page works at all:
//!
//! - **`.wasm` must be `application/wasm`.** `WebAssembly.instantiateStreaming`
//!   refuses anything else, and the failure reads as a broken notebook rather
//!   than as a header problem. This is a real way to spend an afternoon.
//! - **`.js` as `text/javascript`**, because a module script served as
//!   `text/plain` is refused by the browser under `nosniff` — which every
//!   response here carries.
//! - **The fallback is `application/octet-stream`, never a guess.** `nosniff` is
//!   always set, so an unknown type is served as bytes the browser will not
//!   execute. Guessing at content is how a text file becomes a script.

use serde::{Deserialize, Serialize};

/// The file types an attachment field may accept.
///
/// A closed set on purpose: an unknown value in a manifest fails parse rather
/// than admitting a kind nobody reasoned about, which is the same shape as
/// every other enum a stranger's input is matched against. `docx` is
/// deliberately absent — its magic bytes are a zip's (`PK\x03\x04`), so the
/// sniff gate below could not tell it from any other archive, and an
/// allowlist whose gate cannot hold is worse than a shorter allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    Pdf,
    Png,
    #[serde(alias = "jpg")]
    Jpeg,
}

impl FileType {
    /// The canonical media type — ours, derived from the sniffed kind, never
    /// echoed from what an upload claimed about itself.
    pub fn mime(self) -> &'static str {
        match self {
            FileType::Pdf => "application/pdf",
            FileType::Png => "image/png",
            FileType::Jpeg => "image/jpeg",
        }
    }

    /// The extension a stored copy gets. One spelling, chosen by us.
    pub fn extension(self) -> &'static str {
        match self {
            FileType::Pdf => "pdf",
            FileType::Png => "png",
            FileType::Jpeg => "jpg",
        }
    }

    /// What an `<input type="file" accept="…">` should offer for this type —
    /// extensions and mime, comma-joined by the caller.
    pub fn accept_tokens(self) -> &'static [&'static str] {
        match self {
            FileType::Pdf => &[".pdf", "application/pdf"],
            FileType::Png => &[".png", "image/png"],
            FileType::Jpeg => &[".jpg", ".jpeg", "image/jpeg"],
        }
    }

    /// The type whose canonical mime this is, if any — how a validator maps a
    /// stored `content_type` string back to the enum it came from.
    pub fn from_mime(mime: &str) -> Option<FileType> {
        match mime {
            "application/pdf" => Some(FileType::Pdf),
            "image/png" => Some(FileType::Png),
            "image/jpeg" => Some(FileType::Jpeg),
            _ => None,
        }
    }
}

/// What these bytes actually are, by magic number.
///
/// The one thing an unauthenticated upload cannot lie about is its bytes, so
/// this is the gate: the claimed `Content-Type` and the filename's extension
/// are advisory, and a PDF renamed `.png` fails here rather than being stored
/// as what it says it is. Pure and I/O-free like everything in this crate, so
/// the box and home run the identical check.
pub fn sniff(bytes: &[u8]) -> Option<FileType> {
    if bytes.starts_with(b"%PDF-") {
        Some(FileType::Pdf)
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(FileType::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(FileType::Jpeg)
    } else {
        None
    }
}

/// The `Content-Type` for a path inside a bundle.
///
/// Takes the path as it appears on the wire rather than a `Path`, because that
/// is what both servers have and this crate does no I/O.
pub fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "whl" => "application/zip",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "pdf" => "application/pdf",
        // Served as bytes rather than guessed at. Every response carries
        // `nosniff`, so this is the type that runs nothing.
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three that decide whether a page works, rather than how it looks.
    #[test]
    fn the_types_that_are_load_bearing() {
        assert_eq!(content_type("pyodide/pyodide.asm.wasm"), "application/wasm");
        assert_eq!(
            content_type("assets/index-a1b2.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
    }

    /// A file with no extension, or one we do not know, is bytes. It is never
    /// guessed at — guessing is how a text file becomes a script.
    #[test]
    fn the_unknown_is_bytes() {
        assert_eq!(content_type("LICENSE"), "application/octet-stream");
        assert_eq!(content_type("data.parquet"), "application/octet-stream");
        // A dot in a *directory* is not an extension on the file.
        assert_eq!(content_type("v1.2/README"), "application/octet-stream");
    }

    #[test]
    fn case_does_not_change_the_answer() {
        assert_eq!(content_type("A.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type("logo.PNG"), "image/png");
    }

    /// The sniff is a gate, so what matters is what it refuses: a renamed
    /// file, an archive, and anything too short to carry its own magic.
    #[test]
    fn sniff_reads_bytes_not_names() {
        assert_eq!(sniff(b"%PDF-1.7 ..."), Some(FileType::Pdf));
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Some(FileType::Png)
        );
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(FileType::Jpeg));
        // A zip — which is also every docx, xlsx and jar. Refused, which is
        // why docx is not on the allowlist at all.
        assert_eq!(sniff(b"PK\x03\x04"), None);
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"%PD"), None);
        assert_eq!(sniff(b"<html>"), None);
    }

    /// The TOML spellings a manifest may use, including the `jpg` alias —
    /// and the round-trip stays canonical.
    #[test]
    fn file_type_spellings() {
        let parsed: Vec<FileType> = serde_json::from_str(r#"["pdf", "jpg", "jpeg", "png"]"#)
            .expect("all four spellings parse");
        assert_eq!(
            parsed,
            [FileType::Pdf, FileType::Jpeg, FileType::Jpeg, FileType::Png]
        );
        assert_eq!(
            serde_json::to_string(&FileType::Jpeg).unwrap(),
            r#""jpeg""#,
            "the alias parses but never serialises"
        );
        for t in [FileType::Pdf, FileType::Png, FileType::Jpeg] {
            assert_eq!(FileType::from_mime(t.mime()), Some(t));
        }
    }
}
