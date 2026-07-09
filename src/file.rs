//! Built-in `file://` driver and bare-path fallback.
//!
//! ## Percent-encoding
//!
//! When the input carries an explicit `file:` / `file://` scheme prefix
//! the path component is percent-decoded per RFC 3986 §2.1 before being
//! handed to the filesystem. A URI of the form
//! `file:///tmp/foo%20bar.txt` therefore opens `/tmp/foo bar.txt`, as
//! every spec-conformant URI handler would. Bare paths (no scheme) are
//! passed through verbatim so that a real file whose name actually
//! contains a `%` byte is still openable.

use std::fs::File;

use oxideav_core::{BytesSource, Error, Result};

use crate::uri;

/// Open a local file as a `Box<dyn BytesSource>`. Accepts:
/// - bare paths: `/abs/path`, `rel/path`, `Cargo.toml`
///   (passed verbatim — no percent-decoding so a literal `%` in the
///   filename works).
/// - `file:///abs/path`
/// - `file:relative`
///   (percent-decoded — `%20` becomes a space, etc. per RFC 3986 §2.1).
pub fn open_file(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    let (scheme, rest) = uri::split(uri_str);
    if !uri::scheme_is(scheme, "file") {
        return Err(Error::invalid(format!(
            "file driver invoked on non-file URI: {uri_str}"
        )));
    }
    // Percent-decode only when the caller used an explicit `file:` prefix.
    // A bare path of `/tmp/100%-cpu.log` must reach the filesystem with the
    // `%` intact; only URI-form inputs carry the `%HH` escape contract.
    let path: String = if uri::has_file_scheme(uri_str) {
        uri::percent_decode_path(rest)?
    } else {
        rest.to_string()
    };
    let f = File::open(path)?;
    Ok(Box::new(f))
}
