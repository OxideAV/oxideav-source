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

// ───────────────────── symlink escape attempts (Unix) ─────────────────────
//
// The module docs promise "blocks `..` traversals through symlinks":
// every requested path is resolved through `std::fs::canonicalize`
// before consulting the allow/deny lists, so the verdict is always on
// the PHYSICAL location of the target — a symlink can neither smuggle
// an outside file into an allow root nor alias a denied subtree under
// an innocent-looking name. These tests pin that contract with real
// symlinks (Unix-only: `std::os::unix::fs::symlink`).
#[cfg(unix)]
mod symlink_escapes {
    use super::*;
    use std::os::unix::fs::symlink;

    /// Fresh pair of sibling directories `<tag>-root` (the allow root)
    /// and `<tag>-outside` (physically outside it), both cleaned first
    /// so reruns don't trip over stale links.
    fn root_and_outside(tag: &str) -> (PathBuf, PathBuf) {
        let root = tmpdir(&format!("{tag}-root"));
        let outside = tmpdir(&format!("{tag}-outside"));
        for d in [&root, &outside] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        (root, outside)
    }

    #[test]
    fn symlink_to_outside_file_is_rejected() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, outside) = root_and_outside("sym-file-out");
        let secret = outside.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        // Innocent-looking name inside the allow root, physically outside.
        let link = root.join("movie.mp4");
        symlink(&secret, &link).unwrap();

        let scope = FileScope::new().allow_dir(&root);
        let r = scope.resolve(&format!("file://{}", link.display()));
        assert!(
            r.is_err(),
            "symlink inside allow root pointing outside must be rejected"
        );
    }

    #[test]
    fn symlink_to_inside_file_is_admitted() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, _outside) = root_and_outside("sym-file-in");
        let real = root.join("real.bin");
        std::fs::write(&real, b"payload").unwrap();
        let link = root.join("alias.bin");
        symlink(&real, &link).unwrap();

        let scope = FileScope::new().allow_dir(&root);
        let canon = scope
            .resolve(&format!("file://{}", link.display()))
            .expect("symlink to a file physically inside the root must resolve");
        assert_eq!(canon, std::fs::canonicalize(&real).unwrap());
    }

    #[test]
    fn path_through_symlinked_dir_to_outside_is_rejected() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, outside) = root_and_outside("sym-dir-out");
        let secret = outside.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        // root/media -> outside; request root/media/secret.bin.
        let dir_link = root.join("media");
        symlink(&outside, &dir_link).unwrap();

        let scope = FileScope::new().allow_dir(&root);
        let r = scope.resolve(&format!("file://{}/secret.bin", dir_link.display()));
        assert!(
            r.is_err(),
            "path descending through a symlinked dir out of the root must be rejected"
        );
    }

    #[test]
    fn dotdot_through_symlinked_dir_escapes_are_rejected() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, outside) = root_and_outside("sym-dotdot");
        // The bait sits NEXT TO the link target: outside/deep/../bait.bin.
        let deep = outside.join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        let bait = outside.join("bait.bin");
        std::fs::write(&bait, b"bait").unwrap();
        let dir_link = root.join("deep-link");
        symlink(&deep, &dir_link).unwrap();

        // Textually `root/deep-link/../bait.bin` looks like it stays
        // under `root` (`root/bait.bin`); physically the `..` applies to
        // the RESOLVED link target, yielding `outside/bait.bin`.
        // Canonicalise-first semantics must follow the physical route
        // and reject.
        let traversal = dir_link.join("..").join("bait.bin");
        let scope = FileScope::new().allow_dir(&root);
        let r = scope.resolve(&format!("file://{}", traversal.display()));
        assert!(
            r.is_err(),
            "`..` applied after symlink resolution escapes the root and must be rejected"
        );
    }

    #[test]
    fn symlink_alias_into_deny_subtree_is_rejected() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, _outside) = root_and_outside("sym-deny-alias");
        let hole = root.join("private");
        std::fs::create_dir_all(&hole).unwrap();
        let secret = hole.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        // Innocent-looking alias inside the allowed area, physically in
        // the denied subtree.
        let link = root.join("public-looking.bin");
        symlink(&secret, &link).unwrap();

        let scope = FileScope::new().allow_dir(&root).deny_dir(&hole);
        let r = scope.resolve(&format!("file://{}", link.display()));
        assert!(
            r.is_err(),
            "symlink aliasing a deny-listed file must be rejected on its physical location"
        );
    }

    #[test]
    fn symlink_inside_deny_subtree_to_public_file_is_admitted() {
        // Deny verdicts are on the PHYSICAL location too: a link that
        // merely LIVES inside the denied subtree but points at a public
        // file resolves to the public file's canonical path, which the
        // deny list does not cover. Pinned as documented behaviour —
        // the deny list protects the denied bytes, not the namespace.
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, _outside) = root_and_outside("sym-deny-inside");
        let hole = root.join("private");
        std::fs::create_dir_all(&hole).unwrap();
        let public = root.join("public.bin");
        std::fs::write(&public, b"public").unwrap();
        let link = hole.join("escape.bin");
        symlink(&public, &link).unwrap();

        let scope = FileScope::new().allow_dir(&root).deny_dir(&hole);
        let canon = scope
            .resolve(&format!("file://{}", link.display()))
            .expect("link under deny subtree to a public file resolves to the public path");
        assert_eq!(canon, std::fs::canonicalize(&public).unwrap());
    }

    #[test]
    fn percent_encoded_dotdot_traversal_is_rejected() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, outside) = root_and_outside("enc-dotdot");
        let secret = outside.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();

        // file://<root>/%2e%2e/<outside-name>/secret.bin — the escapes
        // decode to `..` (RFC 3986 §2.1) BEFORE canonicalisation, so the
        // scope sees the same traversal as a literal `..` and rejects it.
        let outside_name = outside.file_name().unwrap().to_string_lossy();
        let uri = format!(
            "file://{}/%2e%2e/{}/secret.bin",
            root.display(),
            outside_name
        );
        let scope = FileScope::new().allow_dir(&root);
        let r = scope.resolve(&uri);
        assert!(
            r.is_err(),
            "percent-encoded `..` must decode, canonicalise, and be rejected"
        );
    }

    #[test]
    fn percent_encoded_slash_traversal_is_rejected() {
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, outside) = root_and_outside("enc-slash");
        let secret = outside.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();

        // Separators smuggled entirely through %2F escapes: the decoded
        // path is a plain `<root>/../<outside>/secret.bin` traversal.
        let outside_name = outside.file_name().unwrap().to_string_lossy();
        let uri = format!(
            "file://{}%2F..%2F{}%2Fsecret.bin",
            root.display(),
            outside_name
        );
        let scope = FileScope::new().allow_dir(&root);
        let r = scope.resolve(&uri);
        assert!(
            r.is_err(),
            "%2F-smuggled separators must not bypass the allow-list"
        );
    }

    #[test]
    fn scoped_registry_end_to_end_symlink_escape_rejected() {
        // Same as `symlink_to_outside_file_is_rejected` but through the
        // full registry pipeline (`register_into` + `reg.open`), so the
        // process-global slot path is covered too.
        let _guard = SCOPE_LOCK.lock().unwrap();
        let (root, outside) = root_and_outside("sym-registry-e2e");
        let secret = outside.join("secret.bin");
        std::fs::write(&secret, b"secret").unwrap();
        let link = root.join("innocent.bin");
        symlink(&secret, &link).unwrap();

        let mut reg = SourceRegistry::new();
        FileScope::new().allow_dir(&root).register_into(&mut reg);
        let r = reg.open(&format!("file://{}", link.display()));
        assert!(
            r.is_err(),
            "registry-installed scope must reject the symlink escape"
        );

        // And a legitimate file still opens through the same scope.
        let ok_file = root.join("ok.bin");
        std::fs::write(&ok_file, b"ok-bytes").unwrap();
        let mut r = open_bytes(&reg, &format!("file://{}", ok_file.display()));
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"ok-bytes");
    }
}
