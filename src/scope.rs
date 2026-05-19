//! Security policy for the `file://` driver — directory allow-listing.
//!
//! The default [`open_file`](crate::open_file) opener accepts any path
//! the process can read. That is appropriate for CLI tools where the
//! user picks the file, but unsafe for server processes that take a URI
//! from an external request: a `file:///etc/passwd` URI would otherwise
//! happily resolve.
//!
//! [`FileScope`] holds an allow-list of canonicalised directory roots.
//! A scope-bound opener resolves the requested path through
//! `std::fs::canonicalize` (which follows symlinks and resolves `..`)
//! and rejects anything whose canonical form is not inside one of the
//! allow-listed roots. Install a scope with
//! [`FileScope::register_into`]; from then on, `reg.open("file://…")`
//! is filtered through the scope.
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

/// Configurable allow-list for `file://` opens.
#[derive(Clone, Debug, Default)]
pub struct FileScope {
    /// Canonicalised directory roots. A request resolves to allowed iff
    /// its canonical form starts with one of these roots (path-component
    /// prefix match, not byte-prefix).
    roots: Vec<PathBuf>,
}

impl FileScope {
    /// Empty scope — every open is rejected. Use [`allow_dir`](Self::allow_dir)
    /// to widen.
    pub fn new() -> Self {
        Self::default()
    }

    /// A scope that permits everything the process can read. Equivalent
    /// to the default [`open_file`](crate::open_file) behaviour.
    pub fn permissive() -> Self {
        Self {
            roots: vec![PathBuf::from("/")],
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

    /// Resolve a URI against the scope, returning the canonical absolute
    /// path on success.
    pub fn resolve(&self, uri_str: &str) -> Result<PathBuf> {
        let (scheme, rest) = uri::split(uri_str);
        if scheme != "file" {
            return Err(Error::invalid(format!(
                "FileScope cannot resolve non-file URI: {uri_str}"
            )));
        }
        // Reject paths containing a NUL byte before we even touch the FS.
        if rest.as_bytes().contains(&0u8) {
            return Err(Error::invalid("file path contains NUL byte"));
        }
        // Canonicalise — this follows symlinks and resolves `..`, which
        // is exactly what defeats a `/safe/../etc/passwd` traversal.
        let canon = std::fs::canonicalize(rest)
            .map_err(|e| Error::invalid(format!("file '{rest}' did not canonicalise: {e}")))?;
        if !self.is_allowed(&canon) {
            return Err(Error::invalid(format!(
                "file '{rest}' (canonical '{}') is outside the FileScope allow-list",
                canon.display()
            )));
        }
        Ok(canon)
    }

    /// True iff `canon` lies under at least one allow-listed root,
    /// matched on path components (not bytewise — `/foo` does not
    /// permit `/foobar`).
    fn is_allowed(&self, canon: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| under_root(root.as_path(), canon))
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
}
