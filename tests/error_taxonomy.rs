//! Error-taxonomy coherence across the drivers.
//!
//! Core's `Error` docs define the contract: `InvalidData` for input
//! that violates its format's rules, `Unsupported` for valid input
//! this implementation doesn't cover, `Io` for transport/filesystem
//! failures (with the `io::ErrorKind` carrying the caller-actionable
//! detail). The drivers must pick variants by that rule, not default
//! everything to `InvalidData`:
//!
//! * a well-formed `mem://` URI whose buffer isn't registered is a
//!   lookup miss → `Io(NotFound)`, same shape as a missing file;
//! * a valid URI with a scheme `open_bytes` has no driver for is
//!   `Unsupported`, matching the registry's own miss variant;
//! * a `FileScope` policy rejection is `Io(PermissionDenied)`;
//! * malformed URIs (bad grammar / escapes / base64) stay `InvalidData`.

use std::io::ErrorKind;

use oxideav_core::Error;
use oxideav_source::{open_bytes, open_data, open_file, open_mem, open_slice, FileScope};

fn kind_of(e: &Error) -> Option<ErrorKind> {
    match e {
        Error::Io(io) => Some(io.kind()),
        _ => None,
    }
}

#[test]
fn unregistered_mem_id_is_io_not_found() {
    let e = open_mem("mem://taxonomy-not-registered").err().unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::NotFound),
        "unregistered mem id must be Io(NotFound), got {e:?}"
    );
}

#[test]
fn missing_file_is_io_not_found() {
    let e = open_file("/no/such/oxideav-taxonomy-file").err().unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::NotFound),
        "missing file must pass through as Io(NotFound), got {e:?}"
    );
}

#[test]
fn unknown_scheme_in_open_bytes_is_unsupported() {
    let e = match open_bytes("http://example.com/x") {
        Err(e) => e,
        Ok(_) => panic!("http must not open"),
    };
    assert!(
        matches!(e, Error::Unsupported(_)),
        "unknown scheme must be Unsupported (registry-miss shape), got {e:?}"
    );
}

#[test]
fn scope_policy_rejection_is_permission_denied() {
    let dir = std::env::temp_dir().join("oxideav-taxonomy-scope");
    let hole = dir.join("hole");
    std::fs::create_dir_all(&hole).unwrap();
    let outside = std::env::temp_dir().join("oxideav-taxonomy-outside.bin");
    let inside = hole.join("secret.bin");
    std::fs::write(&outside, b"x").unwrap();
    std::fs::write(&inside, b"s").unwrap();

    // Outside the allow-list.
    let scope = FileScope::new().allow_dir(&dir);
    let e = scope
        .resolve(&format!("file://{}", outside.display()))
        .err()
        .unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::PermissionDenied),
        "allow-list miss must be Io(PermissionDenied), got {e:?}"
    );

    // Inside a deny-listed subtree.
    let scope = FileScope::new().allow_dir(&dir).deny_dir(&hole);
    let e = scope
        .resolve(&format!("file://{}", inside.display()))
        .err()
        .unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::PermissionDenied),
        "deny-list hit must be Io(PermissionDenied), got {e:?}"
    );

    // A path that doesn't exist fails canonicalisation with the
    // underlying kind (NotFound), not a policy or format error.
    let scope = FileScope::permissive();
    let e = scope
        .resolve("file:///no/such/oxideav-taxonomy-canon")
        .err()
        .unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::NotFound),
        "canonicalise failure must keep the IO kind, got {e:?}"
    );
}

#[test]
fn malformed_uris_stay_invalid_data() {
    for (label, e) in [
        ("bad slice grammar", open_slice("slice:nope").err().unwrap()),
        (
            "leading zeros",
            open_slice("slice:007+1!mem://x").err().unwrap(),
        ),
        ("bad escape", open_data("data:,%ZZ").err().unwrap()),
        ("bad base64", open_data("data:;base64,!!!!").err().unwrap()),
        ("missing comma", open_data("data:text/plain").err().unwrap()),
    ] {
        assert!(
            matches!(e, Error::InvalidData(_)),
            "{label} must be InvalidData, got {e:?}"
        );
    }
}

#[test]
fn slice_window_bounds_violation_is_invalid_data() {
    // A window extending past the inner length is a constraint
    // violation of the slice itself — InvalidData, not IO.
    let e = open_slice("slice:5+10!data:,ABC").err().unwrap();
    assert!(
        matches!(e, Error::InvalidData(_)),
        "out-of-bounds window must be InvalidData, got {e:?}"
    );
}

#[test]
fn composite_drivers_propagate_inner_taxonomy() {
    // The NotFound from a missing mem id must survive slice/concat
    // wrapping unchanged.
    let e = open_slice("slice:0+1!mem://taxonomy-absent").err().unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::NotFound),
        "slice must propagate the inner NotFound, got {e:?}"
    );
    let e = open_bytes("concat:data:,ok|mem://taxonomy-absent")
        .err()
        .unwrap();
    assert_eq!(
        kind_of(&e),
        Some(ErrorKind::NotFound),
        "concat must propagate the segment NotFound, got {e:?}"
    );
}
