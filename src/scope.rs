//! Security policy for the `file://` driver — directory allow/deny lists.
//!
//! The default [`open_file`](crate::open_file) opener accepts any path
//! the process can read. That is appropriate for CLI tools where the
//! user picks the file, but unsafe for server processes that take a URI
//! from an external request: a `file:///etc/passwd` URI would otherwise
//! happily resolve.
//!
//! [`FileScope`] holds two canonicalised path-component lists:
//!
//! * **Allow-list** ([`allow_dir`](FileScope::allow_dir)) — a path is
//!   eligible for opening only when its canonical form lies under at
//!   least one allow-listed root. An empty allow-list rejects every
//!   path (the [`permissive`](FileScope::permissive) constructor opts
//!   out of this constraint).
//! * **Deny-list** ([`deny_dir`](FileScope::deny_dir)) — even an
//!   allow-list match (or a permissive scope) is overridden when the
//!   canonical path lies under any deny-listed root. This carves holes
//!   out of an allow-listed root (e.g. allow `/var/media` but never
//!   `/var/media/.snapshots`) and also applies to
//!   [`permissive`](FileScope::permissive) scopes —
//!   `permissive().deny_dir(d)` is "allow anything readable except
//!   inside `d`".
//!
//! A scope-bound opener resolves the requested path through
//! `std::fs::canonicalize` (which follows symlinks and resolves `..`)
//! and consults both lists against that canonical form. Install a scope
//! with [`FileScope::register_into`]; from then on,
//! `reg.open("file://…")` is filtered through the scope.
//!
//! `register_into` plumbs the active scope through a process-global
//! slot because the registry's opener API takes a plain `fn` pointer.
//! A single `FileScope` is therefore active per process at a time;
//! later `register_into` calls overwrite earlier ones.
//!
//! The scope mirrors how container runtimes restrict file:// — there
//! is no external spec; this is operational policy.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use oxideav_core::{BytesSource, Error, Result, SourceRegistry};

use crate::uri;

/// Configurable allow/deny policy for `file://` opens.
#[derive(Clone, Debug, Default)]
pub struct FileScope {
    /// Canonicalised allow-list roots. A request is eligible iff its
    /// canonical form lies under one of these roots (path-component
    /// prefix match, not byte-prefix). Empty + non-permissive = reject
    /// everything.
    roots: Vec<PathBuf>,
    /// Canonicalised deny-list roots. A request is rejected whenever
    /// its canonical form lies under one of these roots, even when the
    /// allow-list (or `permissive`) would otherwise admit it. Same
    /// component-aware prefix match as `roots`.
    denies: Vec<PathBuf>,
    /// If true, the `roots` list is bypassed: any canonicalisable path
    /// is admitted unless the deny-list rejects it. Used by
    /// [`permissive`](Self::permissive) to represent "no restriction
    /// from the allow side" without baking in a Unix-style `/` root
    /// that does not match Windows canonical paths.
    permissive: bool,
}

impl FileScope {
    /// Empty scope — every open is rejected. Use [`allow_dir`](Self::allow_dir)
    /// to widen.
    pub fn new() -> Self {
        Self::default()
    }

    /// A scope that permits everything the process can read. Equivalent
    /// to the default [`open_file`](crate::open_file) behaviour. Useful
    /// where the registry plumbing expects a `FileScope` but the caller
    /// has no security policy to enforce. Pairs with
    /// [`deny_dir`](Self::deny_dir) for "everything except these
    /// roots" policies.
    pub fn permissive() -> Self {
        Self {
            roots: Vec::new(),
            denies: Vec::new(),
            permissive: true,
        }
    }

    /// Permit any path whose canonical form lives under `dir`.
    /// The directory itself is canonicalised at insertion time, so
    /// downstream resolution does not chase symlinks repeatedly.
    pub fn allow_dir<P: AsRef<Path>>(mut self, dir: P) -> Self {
        let canon = std::fs::canonicalize(dir.as_ref()).unwrap_or_else(|_| dir.as_ref().into());
        if !self.roots.iter().any(|r| r == &canon) {
            self.roots.push(canon);
        }
        self
    }

    /// Refuse any path whose canonical form lives under `dir`, even
    /// when the allow-list (or a [`permissive`](Self::permissive)
    /// scope) would otherwise admit it. The directory is canonicalised
    /// at insertion time. Use this to carve a hole out of an
    /// allow-listed root — `allow_dir("/var/media").deny_dir("/var/media/.snapshots")`
    /// admits the broader root but rejects the subtree.
    ///
    /// Deny entries take precedence: if a path is under both an allow
    /// root and a deny root, the deny wins. Component-aware prefix
    /// match — `deny_dir("/foo")` does not affect `/foobar`.
    pub fn deny_dir<P: AsRef<Path>>(mut self, dir: P) -> Self {
        let canon = std::fs::canonicalize(dir.as_ref()).unwrap_or_else(|_| dir.as_ref().into());
        if !self.denies.iter().any(|r| r == &canon) {
            self.denies.push(canon);
        }
        self
    }

    /// Resolve a URI against the scope, returning the canonical absolute
    /// path on success.
    pub fn resolve(&self, uri_str: &str) -> Result<PathBuf> {
        let (scheme, rest) = uri::split(uri_str);
        if !uri::scheme_is(scheme, "file") {
            return Err(Error::invalid(format!(
                "FileScope cannot resolve non-file URI: {uri_str}"
            )));
        }
        // Percent-decode the path component when the caller used an
        // explicit `file:` / `file://` prefix (matches `open_file`). Bare
        // paths are passed verbatim so a real `%` in the filename works.
        // Done before the NUL check so a smuggled `%00` is also caught.
        let decoded: String = if uri::has_file_scheme(uri_str) {
            uri::percent_decode_path(rest)?
        } else {
            rest.to_string()
        };
        // Reject paths containing a NUL byte before we even touch the FS.
        if decoded.as_bytes().contains(&0u8) {
            return Err(Error::invalid("file path contains NUL byte"));
        }
        // Canonicalise — this follows symlinks and resolves `..`, which
        // is exactly what defeats a `/safe/../etc/passwd` traversal.
        // Taxonomy: canonicalisation failure is a filesystem miss (the
        // path typically does not exist) — keep the underlying IO kind
        // rather than reporting malformed input.
        let canon = std::fs::canonicalize(&decoded).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("file '{decoded}' did not canonicalise: {e}"),
            ))
        })?;
        // Taxonomy: policy rejections are PermissionDenied — the path
        // is well-formed and may even exist; this scope refuses it.
        if self.is_denied(&canon) {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "file '{decoded}' (canonical '{}') is inside a FileScope deny-listed root",
                    canon.display()
                ),
            )));
        }
        if !self.is_allow_listed(&canon) {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "file '{decoded}' (canonical '{}') is outside the FileScope allow-list",
                    canon.display()
                ),
            )));
        }
        Ok(canon)
    }

    /// True iff `canon` lies under at least one allow-listed root,
    /// matched on path components (not bytewise — `/foo` does not
    /// permit `/foobar`). A `permissive` scope always returns true on
    /// this check; the deny-list is consulted separately.
    fn is_allow_listed(&self, canon: &Path) -> bool {
        if self.permissive {
            return true;
        }
        self.roots
            .iter()
            .any(|root| under_root(root.as_path(), canon))
    }

    /// True iff `canon` lies under at least one deny-listed root.
    fn is_denied(&self, canon: &Path) -> bool {
        self.denies
            .iter()
            .any(|root| under_root(root.as_path(), canon))
    }

    /// Combined verdict: returns true iff this scope would admit
    /// `canon` (allow-listed and not deny-listed). Useful for
    /// callers that want to test a path without actually opening it.
    /// The path is taken as-is; canonicalise it yourself if you want
    /// symlink / `..` resolution.
    pub fn is_allowed_path(&self, canon: &Path) -> bool {
        !self.is_denied(canon) && self.is_allow_listed(canon)
    }

    /// Open `uri_str` under this scope.
    pub fn open(&self, uri_str: &str) -> Result<Box<dyn BytesSource>> {
        let canon = self.resolve(uri_str)?;
        let f = File::open(canon)?;
        Ok(Box::new(f))
    }

    /// Install this scope as the `file://` driver of `registry`,
    /// **replacing** any prior `file` registration. The scope is stored
    /// in a process-global slot keyed by `registry` registration order;
    /// subsequent `register_into` calls overwrite that slot.
    pub fn register_into(self, registry: &mut SourceRegistry) {
        *active().write().expect("FileScope slot poisoned") = Some(Arc::new(self));
        registry.register_bytes("file", open_file_scoped);
    }
}

/// Process-global "current scope" used by [`open_file_scoped`]. Source
/// registry opener functions must be plain `fn` pointers (not closures),
/// so we keep the scope in a slot the opener can look up.
fn active() -> &'static RwLock<Option<Arc<FileScope>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<FileScope>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Free-function opener compatible with `SourceRegistry::register_bytes`.
/// Looks up the active scope and delegates. If no scope is installed,
/// every call errors — install via [`FileScope::register_into`] first.
pub fn open_file_scoped(uri_str: &str) -> Result<Box<dyn BytesSource>> {
    let slot = active().read().expect("FileScope slot poisoned");
    let scope = slot
        .as_ref()
        .ok_or_else(|| Error::invalid("file driver: no FileScope installed"))?
        .clone();
    drop(slot);
    scope.open(uri_str)
}

/// True iff `child` lies at or under `root`, compared component-wise.
fn under_root(root: &Path, child: &Path) -> bool {
    let mut r = root.components();
    let mut c = child.components();
    loop {
        match (r.next(), c.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(_), _) => return false,
            (None, _) => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn tmp_file(name: &str, body: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("oxideav-source-scope-{name}"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body).unwrap();
        p
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("oxideav-source-scope-d-{name}"));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    #[test]
    fn empty_scope_rejects_everything() {
        let path = tmp_file("empty-rejects", b"x");
        let scope = FileScope::new();
        let r = scope.resolve(&format!("file://{}", path.display()));
        assert!(r.is_err());
    }

    #[test]
    fn allow_dir_admits_files_inside() {
        let dir = tmp_dir("allow-admits");
        let file = dir.join("a.bin");
        std::fs::write(&file, b"hello").unwrap();
        let scope = FileScope::new().allow_dir(&dir);
        let canon = scope
            .resolve(&format!("file://{}", file.display()))
            .unwrap();
        // canonicalise differs from raw path on macOS (/private/var/...);
        // require the *file* path to canonicalise equally.
        assert_eq!(canon, std::fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn traversal_blocked_after_canonicalisation() {
        let dir = tmp_dir("traversal-blocked");
        // Outside file:
        let outside = tmp_file("traversal-outside", b"secret");
        // The traversal path: <dir>/../<file>
        let traversal = dir.join("..").join(outside.file_name().unwrap());
        let scope = FileScope::new().allow_dir(&dir);
        let r = scope.resolve(&format!("file://{}", traversal.display()));
        assert!(r.is_err(), "traversal must be rejected");
    }

    #[test]
    fn prefix_match_is_component_aware() {
        let parent = tmp_dir("prefix-component-aware-parent");
        // /tmp/.../parent_extra — bytewise prefix-matches parent but is a
        // different directory.
        let mut extra: PathBuf = parent.clone();
        extra.set_file_name(format!(
            "{}_extra",
            parent.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&extra).unwrap();
        let outside = extra.join("file.bin");
        std::fs::write(&outside, b"x").unwrap();
        let scope = FileScope::new().allow_dir(&parent);
        let r = scope.resolve(&format!("file://{}", outside.display()));
        assert!(
            r.is_err(),
            "component-aware match must reject sibling dir whose name shares a prefix"
        );
    }

    #[test]
    fn permissive_admits_anything_readable() {
        let p = tmp_file("permissive", b"abc");
        let scope = FileScope::permissive();
        let r = scope.resolve(&format!("file://{}", p.display()));
        assert!(r.is_ok());
    }

    #[test]
    fn nul_byte_rejected() {
        let scope = FileScope::permissive();
        let r = scope.resolve("file:///tmp/a\0b");
        assert!(r.is_err());
    }

    #[test]
    fn non_file_scheme_rejected() {
        let scope = FileScope::permissive();
        let r = scope.resolve("http://example.com/x");
        assert!(r.is_err());
    }

    #[test]
    fn open_reads_file_under_allowed_dir() {
        let dir = tmp_dir("open-reads");
        let file = dir.join("payload.bin");
        std::fs::write(&file, b"payload!").unwrap();
        let scope = FileScope::new().allow_dir(&dir);
        let mut r = scope.open(&format!("file://{}", file.display())).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut r, &mut buf).unwrap();
        assert_eq!(buf, b"payload!");
    }

    #[test]
    fn deny_dir_carves_a_hole_inside_allow_root() {
        let root = tmp_dir("deny-carve-root");
        let hole = root.join("private");
        std::fs::create_dir_all(&hole).unwrap();
        let public_file = root.join("public.bin");
        let private_file = hole.join("secret.bin");
        std::fs::write(&public_file, b"public").unwrap();
        std::fs::write(&private_file, b"secret").unwrap();

        let scope = FileScope::new().allow_dir(&root).deny_dir(&hole);

        // Inside the allow root and outside the deny subtree — admitted.
        let ok = scope.resolve(&format!("file://{}", public_file.display()));
        assert!(ok.is_ok(), "public file under allow root must be admitted");

        // Inside the deny subtree — rejected even though it's under the allow root.
        let blocked = scope.resolve(&format!("file://{}", private_file.display()));
        assert!(
            blocked.is_err(),
            "file inside deny-listed subtree must be rejected"
        );
    }

    #[test]
    fn deny_takes_precedence_over_permissive() {
        let hole = tmp_dir("deny-vs-permissive-hole");
        let inside = hole.join("file.bin");
        std::fs::write(&inside, b"x").unwrap();
        let outside = tmp_file("deny-vs-permissive-outside", b"y");

        let scope = FileScope::permissive().deny_dir(&hole);

        // Outside the deny subtree — still admitted under permissive.
        let ok = scope.resolve(&format!("file://{}", outside.display()));
        assert!(
            ok.is_ok(),
            "permissive scope without deny match must still admit"
        );

        // Inside the deny subtree — rejected despite permissive.
        let blocked = scope.resolve(&format!("file://{}", inside.display()));
        assert!(blocked.is_err(), "deny must override permissive admission");
    }

    #[test]
    fn deny_dir_alone_without_allow_still_rejects_everything() {
        // Deny without allow == still empty allow-list == reject all.
        let hole = tmp_dir("deny-without-allow");
        let inside = hole.join("file.bin");
        std::fs::write(&inside, b"x").unwrap();
        let scope = FileScope::new().deny_dir(&hole);
        let r = scope.resolve(&format!("file://{}", inside.display()));
        assert!(
            r.is_err(),
            "deny-only scope has empty allow-list and must reject"
        );
    }

    #[test]
    fn deny_dir_component_aware() {
        // /tmp/.../hole vs /tmp/.../hole_extra — bytewise-prefix match
        // but not a component-prefix; deny on `hole` must NOT cover
        // `hole_extra`.
        let allow_root = tmp_dir("deny-comp-aware-allow");
        let hole = allow_root.join("hole");
        let neighbour = allow_root.join("hole_extra");
        std::fs::create_dir_all(&hole).unwrap();
        std::fs::create_dir_all(&neighbour).unwrap();
        let neighbour_file = neighbour.join("file.bin");
        std::fs::write(&neighbour_file, b"x").unwrap();

        let scope = FileScope::new().allow_dir(&allow_root).deny_dir(&hole);
        let r = scope.resolve(&format!("file://{}", neighbour_file.display()));
        assert!(
            r.is_ok(),
            "deny on 'hole' must not cover the sibling 'hole_extra'"
        );
    }

    #[test]
    fn duplicate_deny_dir_is_idempotent() {
        let allow_root = tmp_dir("deny-dedup-allow");
        let hole = allow_root.join("hole");
        std::fs::create_dir_all(&hole).unwrap();
        let scope = FileScope::new()
            .allow_dir(&allow_root)
            .deny_dir(&hole)
            .deny_dir(&hole);
        assert_eq!(scope.denies.len(), 1);
    }

    #[test]
    fn scope_resolve_percent_decodes_file_url() {
        // A scope must percent-decode `%HH` in the URI form before
        // canonicalising / checking against the allow-list.
        let dir = tmp_dir("scope-percent-decodes");
        let file = dir.join("name with space.bin");
        std::fs::write(&file, b"ok").unwrap();
        let scope = FileScope::new().allow_dir(&dir);

        // Encode the space as %20 per RFC 3986 §2.1.
        let encoded = file.display().to_string().replace(' ', "%20");
        let canon = scope.resolve(&format!("file://{encoded}")).unwrap();
        assert_eq!(canon, std::fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn scope_resolve_percent_decoded_nul_rejected() {
        // Smuggling a NUL byte via `%00` must be caught after decoding,
        // before the path reaches the filesystem.
        let scope = FileScope::permissive();
        let r = scope.resolve("file:///tmp/x%00y");
        assert!(r.is_err());
    }

    #[test]
    fn scope_resolve_bare_path_does_not_percent_decode() {
        // A bare path is taken verbatim. The test file's name actually
        // contains `%20`; the scope must not decode it.
        let dir = tmp_dir("scope-bare-no-decode");
        let file = dir.join("100%20raw.bin");
        std::fs::write(&file, b"x").unwrap();
        let scope = FileScope::new().allow_dir(&dir);
        // Bare path form (no scheme) — verbatim resolution.
        let canon = scope.resolve(&file.display().to_string()).unwrap();
        assert_eq!(canon, std::fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn is_allowed_path_inspector_matches_resolve() {
        let allow_root = tmp_dir("inspector-allow");
        let hole = allow_root.join("hole");
        std::fs::create_dir_all(&hole).unwrap();
        let public_file = allow_root.join("public.bin");
        let private_file = hole.join("secret.bin");
        std::fs::write(&public_file, b"p").unwrap();
        std::fs::write(&private_file, b"s").unwrap();

        let scope = FileScope::new().allow_dir(&allow_root).deny_dir(&hole);

        let public_canon = std::fs::canonicalize(&public_file).unwrap();
        let private_canon = std::fs::canonicalize(&private_file).unwrap();
        assert!(scope.is_allowed_path(&public_canon));
        assert!(!scope.is_allowed_path(&private_canon));
    }
}
