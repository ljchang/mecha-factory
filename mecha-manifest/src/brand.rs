//! The mark, for the surfaces this crate renders.
//!
//! Exactly one asset lives here — the favicon — and it is a `data:` URI rather
//! than a file for the reason that governs everything else on these surfaces:
//! **a served page and a published bundle both have to be self-contained.** A
//! `<link rel="icon" href="/favicon.svg">` is an external reference. The
//! publish gate fails a bundle for one, and a report opened from disk or
//! re-hosted somewhere else would silently lose it.
//!
//! `img-src 'self' data:` is in every CSP class this crate emits, so nothing
//! about the policy has to change to allow it. That is the whole reason a data
//! URI is the shape here rather than an inline `<svg>`: a favicon has to be a
//! URL, and this is the only kind of URL that carries its own bytes.
//!
//! It is the 16px build of the mark from mecha's `brand/favicon.svg` — the one
//! with blunted feet, snapped to a 4-unit grid so no edge lands mid-pixel — and
//! it carries both schemes itself. A favicon sits in browser chrome, not on our
//! page, so it cannot inherit a theme; accent-700 on light and accent-400 on
//! dark is the brand's own light-ground rule, applied where CSS is the only way
//! to ask.

/// The mark as a `data:` URI, ready for `<link rel="icon" href="…">`.
///
/// Percent-encoded rather than raw: `<`, `>` and `#` are not legal unescaped
/// in a URI, and a favicon that works in one browser and not another is worse
/// than none. Readable source, for anyone editing it:
///
/// ```svg
/// <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
///   <style>
///     path { fill: #5d5294 }
///     @media (prefers-color-scheme: dark) { path { fill: #b5abfc } }
///   </style>
///   <path d="M0 0h24l8 10 8-10h24v20H0z"/>
///   <path d="M0 24h16v16H0zM48 24h16v16H48zM0 44h16v20H0zM48 44h16v20H48z"/>
///   <path d="M16 44v20h12zM48 44v20H36z"/>
///   <path d="M20 28h24v8H20z"/>
/// </svg>
/// ```
/// The bytes, once. `concat!` takes literals and not constants, so the two
/// constants below would otherwise be two copies of the same SVG — and the
/// copy that did not get edited is the one that ships.
macro_rules! favicon_uri {
    () => {
        concat!(
            "data:image/svg+xml,",
            "%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2064%2064'%3E",
            "%3Cstyle%3Epath%7Bfill:%235d5294%7D",
            "@media(prefers-color-scheme:dark)%7Bpath%7Bfill:%23b5abfc%7D%7D%3C/style%3E",
            "%3Cpath%20d='M0%200h24l8%2010%208-10h24v20H0z'/%3E",
            "%3Cpath%20d='M0%2024h16v16H0zM48%2024h16v16H48zM0%2044h16v20H0zM48%2044h16v20H48z'/%3E",
            "%3Cpath%20d='M16%2044v20h12zM48%2044v20H36z'/%3E",
            "%3Cpath%20d='M20%2028h24v8H20z'/%3E",
            "%3C/svg%3E",
        )
    };
}

pub const FAVICON_DATA_URI: &str = favicon_uri!();

/// The `<link>` element, since both callers want the same one.
pub const FAVICON_LINK: &str = concat!("<link rel=\"icon\" href=\"", favicon_uri!(), "\">");

/// The mark as inline SVG, `fill="currentColor"` — `brand/logo-mono.svg`
/// verbatim.
///
/// Inline rather than served, for the favicon's reason wearing page clothes:
/// no asset route means no new URL whose resolution could distinguish pages
/// that must answer identically, and `currentColor` inherits `--text`, so
/// one asset is correct in both themes with no media query. Attribute-fill
/// only, no `<style>` block — `style-src 'self'` stays untouched.
pub const LOGO_MONO_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" \
viewBox=\"0 0 63 54\" width=\"28\" height=\"24\" fill=\"currentColor\" \
role=\"img\" aria-label=\"mecha\">\
<path d=\"M0 0h24l7.5 8.5L39 0h24v16H0z\"></path>\
<path d=\"M0 20h14v15H0zM49 20h14v15H49zM0 39h14v15H0zM49 39h14v15H49z\"></path>\
<path d=\"M14 39v15h13.24zM49 39v15H35.76z\"></path>\
<path d=\"M21 24h21v7H21z\"></path>\
</svg>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_embeds_the_uri() {
        assert!(FAVICON_LINK.contains(FAVICON_DATA_URI));
        assert!(FAVICON_LINK.starts_with("<link rel=\"icon\" href=\""));
        assert!(FAVICON_LINK.ends_with("\">"));
    }

    /// A raw `<`, `>` or `#` is what breaks this in one browser and not
    /// another, and `"` would close the attribute it sits in.
    #[test]
    fn nothing_in_the_uri_needs_escaping() {
        for bad in ['<', '>', '#', '"', ' '] {
            assert!(
                !FAVICON_DATA_URI.contains(bad),
                "{bad:?} is unescaped in the favicon URI"
            );
        }
    }

    /// Both schemes, always — the same rule the themes follow. A favicon sits
    /// in browser chrome and cannot inherit one.
    #[test]
    fn it_carries_both_schemes() {
        assert!(FAVICON_DATA_URI.contains("prefers-color-scheme:dark"));
        assert!(FAVICON_DATA_URI.contains("%235d5294"), "accent-700, light");
        assert!(FAVICON_DATA_URI.contains("%23b5abfc"), "accent-400, dark");
    }
}
