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
}
