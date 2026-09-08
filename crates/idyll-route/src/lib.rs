//! Typed routes.
//!
//! A URL is a **projection** of a typed route value, never the source of truth. A `Route`
//! you can hold is a page that exists — nothing to validate — and its [`url`](Route::url) is
//! one rendering of it. The dual, [`parse`](Route::parse), recovers the route from a request
//! path. `#[derive(Route)]` emits **both** directions from one `#[route("…")]` spec per
//! variant, so they are inverse by construction and cannot drift:
//!
//! ```ignore
//! #[derive(Route)]
//! enum BlogRoute {
//!     #[route("/")]              Index,
//!     #[route("/posts/{slug}")]  Post { slug: String },       // one segment  → String
//!     #[route("/docs/{path*}")]  Doc  { path: Vec<String> },  // catch-all    → Vec<String>
//! }
//! ```
//!
//! A `{seg}` binds exactly one path segment (`String`); a `{seg*}` binds the rest as segments
//! (`Vec<String>`). Segments are the honest unit: percent-encoding is per-segment, so a slug
//! containing a `/` (encoded `%2F`) stays one segment and round-trips — a flat `String` would
//! lose that boundary. Enumerating *which* routes exist is a separate, data-dependent concern
//! (see `idyll_serve::Sitemap`); this crate is only the pure `url ↔ route` codec.

use std::fmt;

/// `#[derive(Route)]` — implements [`Route`] from `#[route("…")]` patterns (re-exported
/// serde-style, so `idyll_route::Route` names both the trait and its derive).
pub use idyll_macros::Route;

/// A relative URL path — the projection of a typed [`Route`]. Built from decoded segments,
/// each percent-encoded and joined under a leading `/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Url(String);

impl Url {
    /// Build a path from decoded segments: each is percent-encoded and joined with `/` under a
    /// leading `/`. No segments is the root, `/`.
    pub fn from_segments<'a>(segments: impl IntoIterator<Item = &'a str>) -> Url {
        let mut s = String::from("/");
        for (i, seg) in segments.into_iter().enumerate() {
            if i > 0 {
                s.push('/');
            }
            s.push_str(&encode_segment(seg));
        }
        Url(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The bidirectional route codec. `#[derive(Route)]` implements it; both directions fall out
/// of the one `#[route]` spec, so [`url`](Route::url) is the inverse of [`parse`](Route::parse).
pub trait Route: Sized {
    /// url → route. `None` when no variant's pattern matches — the typed 404.
    fn parse(path: &str) -> Option<Self>;
    /// route → url. Total: a route value is a page that exists.
    fn url(&self) -> Url;
}

/// Split a path into its non-empty segments (`"/docs/a/b"` → `["docs","a","b"]`, `"/"` → `[]`).
/// Leading/trailing slashes and empty segments are dropped; the derive's matchers work over
/// this normalized form. (Segments stay percent-encoded here — the derive decodes per binding.)
pub fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// The bytes to percent-encode in a path segment: everything outside the RFC 3986 *unreserved*
/// set (`ALPHA` / `DIGIT` / `-` / `_` / `.` / `~`). Crucially this includes `/`, so a segment
/// that contains a slash encodes it (`%2F`) and stays one segment.
const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encode one path segment.
pub fn encode_segment(seg: &str) -> String {
    percent_encoding::utf8_percent_encode(seg, SEGMENT).to_string()
}

/// Percent-decode one path segment. `None` on invalid UTF-8.
pub fn decode_segment(seg: &str) -> Option<String> {
    percent_encoding::percent_decode_str(seg).decode_utf8().ok().map(|c| c.into_owned())
}
