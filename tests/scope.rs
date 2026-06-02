//! `FileScope` integration tests — directory allow-list via the
//! registry pipeline.
//!
//! NOTE: `FileScope::register_into` plumbs the active scope through a
//! process-global slot (the registry opener must be a plain `fn`).
//! These tests therefore share a `Mutex` so they cannot race each
//! other on that slot.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

use oxideav_source::{BytesSource, FileScope, SourceOutput, SourceRegistry};

/// Serialises every test in this file — see module-level note.
static SCOPE_LOCK: Mutex<()> = Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("oxideav-source-scope-int-{tag}"));
    let _ = std::fs::create_dir_all(&p);
    p
}

fn open_bytes(reg: &SourceRegistry, uri: &str) -> Box<dyn BytesSource> {
    match reg.open(uri).expect("open") {
        SourceOutput::Bytes(b) => b,
        _ => panic!("expected SourceOutput::Bytes from file driver"),
    }
}

#[test]
fn scoped_registry_allows_paths_inside_root() {
    let _guard = SCOPE_LOCK.lock().unwrap();
    let dir = tmpdir("inside-root");
    let file = dir.join("payload.bin");
    std::fs::write(&file, b"inside-root-payload").unwrap();

    let mut reg = SourceRegistry::new();
    FileScope::new().allow_dir(&dir).register_into(&mut reg);

    let mut r = open_bytes(&reg, &format!("file://{}", file.display()));
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"inside-root-payload");
}

#[test]
fn scoped_registry_rejects_paths_outside_root() {
    let _guard = SCOPE_LOCK.lock().unwrap();
    let dir_in = tmpdir("rejects-outside-in");
    let dir_out = tmpdir("rejects-outside-out");
    let outside = dir_out.join("secret.bin");
    std::fs::write(&outside, b"secret").unwrap();

    let mut reg = SourceRegistry::new();
    FileScope::new().allow_dir(&dir_in).register_into(&mut reg);

    let r = reg.open(&format!("file://{}", outside.display()));
    assert!(r.is_err(), "outside path must be rejected by scope");
}

#[test]
fn scoped_registry_rejects_traversal() {
    let _guard = SCOPE_LOCK.lock().unwrap();
    let dir = tmpdir("rejects-traversal");
    // /tmp/<dir>/../<sibling-name> — sibling, but addressed through the
    // allowed root.
    let outside_dir = tmpdir("rejects-traversal-sibling");
    let outside = outside_dir.join("bait.bin");
    std::fs::write(&outside, b"bait").unwrap();
    let traversal = dir.join("..").join(format!(
        "{}/{}",
        outside_dir.file_name().unwrap().to_string_lossy(),
        outside.file_name().unwrap().to_string_lossy()
    ));

    let mut reg = SourceRegistry::new();
    FileScope::new().allow_dir(&dir).register_into(&mut reg);

    let r = reg.open(&format!("file://{}", traversal.display()));
    assert!(
        r.is_err(),
        "traversal through allowed-root must be rejected"
    );
}

#[test]
fn scoped_registry_deny_dir_carves_hole_inside_allow_root() {
    let _guard = SCOPE_LOCK.lock().unwrap();
    let root = tmpdir("deny-carve-root");
    let hole = root.join("private");
    let _ = std::fs::create_dir_all(&hole);
    let public_file = root.join("public.bin");
    let private_file = hole.join("secret.bin");
    std::fs::write(&public_file, b"public-payload").unwrap();
    std::fs::write(&private_file, b"secret-payload").unwrap();

    let mut reg = SourceRegistry::new();
    FileScope::new()
        .allow_dir(&root)
        .deny_dir(&hole)
        .register_into(&mut reg);

    // Public file under allow root — admitted, contents reach us.
    let mut r = open_bytes(&reg, &format!("file://{}", public_file.display()));
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"public-payload");

    // File under the deny subtree — rejected even though it is also
    // under the allow root.
    let err = reg.open(&format!("file://{}", private_file.display()));
    assert!(
        err.is_err(),
        "registry must reject file inside deny-listed subtree"
    );
}

#[test]
fn scoped_registry_deny_traversal_into_hole_is_blocked() {
    // Address a denied file via a traversal path. Canonicalisation
    // resolves the `..`, so the deny-list still catches it.
    let _guard = SCOPE_LOCK.lock().unwrap();
    let root = tmpdir("deny-traversal-root");
    let hole = root.join("private");
    let _ = std::fs::create_dir_all(&hole);
    let private_file = hole.join("secret.bin");
    std::fs::write(&private_file, b"secret").unwrap();

    // /tmp/.../root/public-decoy/../private/secret.bin — the path
    // segment "public-decoy" doesn't need to exist for canonicalize
    // resolution of `..`; what matters is the post-canonicalise form.
    // Build a real intermediate so canonicalize works.
    let decoy = root.join("public-decoy");
    let _ = std::fs::create_dir_all(&decoy);
    let traversal = decoy.join("..").join("private").join("secret.bin");

    let mut reg = SourceRegistry::new();
    FileScope::new()
        .allow_dir(&root)
        .deny_dir(&hole)
        .register_into(&mut reg);

    let r = reg.open(&format!("file://{}", traversal.display()));
    assert!(
        r.is_err(),
        "traversal into deny-listed subtree must be rejected"
    );
}
