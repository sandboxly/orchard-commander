//! Serializable descriptions of places ("specs"), for bookmarks and the like.
//!
//! The specs themselves are shared with the other Orchard apps
//! ([`orchard_places::locations`], re-exported here). A live location is an
//! [`oc_vfs::VPath`], which holds open file systems and can't be saved:
//! [`spec_of`] describes one as a spec, so the way back can be saved.

use oc_vfs::{FileSystem, VPath};

pub use orchard_places::locations::*;

/// What the app knows about its file systems that [`FileSystem`] doesn't say.
pub trait SpecContext {
    /// Whether this is the local disk (paths are OS paths). Default: what
    /// the file system says ([`FileSystem::is_local`]).
    fn is_local(&self, fs: &dyn FileSystem) -> bool {
        fs.is_local()
    }
    /// The connection account of a server file system, if it is one the app
    /// knows (and can therefore reconnect to).
    fn account_of(&self, fs: &dyn FileSystem) -> Option<String>;
}

/// A spec for `location`, or `None` if it can't be described (e.g. a server
/// connection the context doesn't know).
pub fn spec_of(location: &VPath, ctx: &dyn SpecContext) -> Option<Spec> {
    let fs = location.fs.as_ref();
    if let Some(container) = fs.container() {
        let inner = spec_of(&container, ctx)?;
        return Some(Spec::archive(inner, location.path.clone()));
    }
    if ctx.is_local(fs) {
        return Some(Spec::local(location.path.clone()));
    }
    ctx.account_of(fs)
        .map(|account| Spec::server(account, location.path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_vfs::{Entry, ListOptions, ReadStream, RelPath};
    use std::sync::Arc;

    fn local(p: &str) -> Spec {
        Spec::local(p)
    }

    // -- spec_of with stand-in file systems -------------------------------------

    struct Fake {
        name: &'static str,
        container: Option<VPath>,
    }

    impl FileSystem for Fake {
        fn type_name(&self) -> String {
            self.name.into()
        }
        fn container(&self) -> Option<VPath> {
            self.container.clone()
        }
        fn list_dir(&self, _: &str, _: ListOptions) -> oc_vfs::Result<Vec<Entry>> {
            Ok(Vec::new())
        }
        fn stat(&self, p: &str) -> oc_vfs::Result<Entry> {
            Err(oc_vfs::VfsError::NotFound(p.into()))
        }
        fn parent_path(&self, _: &str) -> Option<String> {
            None
        }
        fn join(&self, path: &str, names: &[&str]) -> String {
            std::iter::once(path)
                .chain(names.iter().copied())
                .collect::<Vec<_>>()
                .join("/")
        }
        fn basename(&self, path: &str) -> String {
            path.rsplit('/').next().unwrap_or(path).into()
        }
        fn display(&self, path: &str) -> String {
            path.into()
        }
        fn is_local(&self) -> bool {
            self.name == "local"
        }
        fn open_read(&self, p: &str) -> oc_vfs::Result<Box<dyn ReadStream>> {
            Err(oc_vfs::VfsError::NotFound(p.into()))
        }
        fn walk<'a>(
            &'a self,
            _: &str,
        ) -> oc_vfs::Result<Box<dyn Iterator<Item = oc_vfs::Result<(RelPath, Entry)>> + 'a>>
        {
            Ok(Box::new(std::iter::empty()))
        }
    }

    struct Ctx(Option<&'static str>);

    impl SpecContext for Ctx {
        fn account_of(&self, _: &dyn FileSystem) -> Option<String> {
            self.0.map(String::from)
        }
    }

    fn fs(name: &'static str, container: Option<VPath>) -> Arc<dyn FileSystem> {
        Arc::new(Fake { name, container })
    }

    #[test]
    fn specs_for_local_archive_and_server() {
        let disk = fs("local", None);
        let none = Ctx(None);
        assert_eq!(
            spec_of(&VPath::new(disk.clone(), "/tmp/x"), &none),
            Some(local("/tmp/x"))
        );

        let zip = fs("zip", Some(VPath::new(disk, "/tmp/x/pack.zip")));
        let inside = spec_of(&VPath::new(zip, "docs"), &none).unwrap();
        assert_eq!(inside, Spec::archive(local("/tmp/x/pack.zip"), "docs"));
        assert!(inside.as_ref().describe().ends_with("pack.zip/docs"));
        assert_eq!(inside.name(), "docs");

        let server = fs("mem", None);
        let on_server = VPath::new(server.clone(), "projects");
        assert_eq!(spec_of(&on_server, &none), None); // unknown connection: can't be bookmarked
        let known = Ctx(Some("mem://me@bucket"));
        let s = spec_of(&on_server, &known).unwrap();
        assert_eq!(s, Spec::server("mem://me@bucket", "projects"));
        assert_eq!(s.describe(), "mem://me@bucket/projects");

        let nested = fs("zip", Some(VPath::new(server, "nested.zip")));
        let s = spec_of(&VPath::new(nested.clone(), ""), &known).unwrap();
        assert_eq!(
            s.0["archive"],
            Spec::server("mem://me@bucket", "nested.zip").into_value()
        );
        assert_eq!(spec_of(&VPath::new(nested, ""), &none), None);
    }
}
