//! Location specs <-> live folders (the app side of `oc_config::locations`).
//!
//! * [`spec_of`]: a spec for a panel's folder. Server folders need their
//!   connection account; the connections feature tells it through
//!   [`register_account_of`].
//! * [`resolve`]: a spec back to a [`VPath`], asynchronously: local folders
//!   right away, archives opened on a worker (nested ones one after the
//!   other), servers connected by `connections::api::resolve_server`.

use oc_config::locations::{SpecContext, Target};
use oc_config::{Spec, SpecRef};
use oc_vfs::{FileSystem, VPath};

use crate::app::App;

/// The connection account of a server file system, if the connections
/// feature knows it (1.x: `sessions.owner_of`).
pub type AccountOf = fn(&App, &dyn FileSystem) -> Option<String>;

#[derive(Default)]
struct AccountHooks(Vec<AccountOf>);

/// For the connections feature (call from its `on_start`): how to find a
/// server file system's account, so server folders can be bookmarked.
pub fn register_account_of(app: &mut App, f: AccountOf) {
    app.state::<AccountHooks>().0.push(f);
}

struct Ctx<'a>(&'a App);

impl SpecContext for Ctx<'_> {
    fn account_of(&self, fs: &dyn FileSystem) -> Option<String> {
        self.0.try_state::<AccountHooks>()?.0.iter().find_map(|f| f(self.0, fs))
    }
}

/// A spec for `location`, or `None` if it can't be described (a search
/// result list, a server the connections feature doesn't know).
pub fn spec_of(app: &App, location: &VPath) -> Option<Spec> {
    oc_config::locations::spec_of(location, &Ctx(app))
}

/// Gets the live location (or an error message) on the UI thread.
pub type Resolved = Box<dyn FnOnce(&mut App, Result<VPath, String>) + Send>;

/// Turn a spec back into a live location (asynchronously), then call `on_done`.
pub fn resolve(app: &mut App, spec: &Spec, on_done: Resolved) {
    resolve_ref(app, spec.as_ref(), on_done);
}

fn resolve_ref(app: &mut App, spec: SpecRef<'_>, on_done: Resolved) {
    match spec.target() {
        None => on_done(app, Err("This bookmark is damaged and can't be opened".into())),
        Some(Target::Local { path }) => on_done(app, Ok(oc_local::local(path))),
        Some(Target::Server { account, path }) => {
            crate::connections::api::resolve_server(app, account, path, move |app, result| on_done(app, result));
        }
        Some(Target::Archive { archive, path }) => {
            // First the archive file, then its contents.
            let inner = path.to_owned();
            resolve_ref(
                app,
                archive,
                Box::new(move |app, container| {
                    let container = match container {
                        Ok(c) => c,
                        Err(e) => return on_done(app, Err(e)),
                    };
                    let name = container.name();
                    app.tasks.run_in_background(
                        move |_| oc_vfs::open_archive(&container),
                        move |app, opened| {
                            let result = match opened {
                                Some(Ok(root)) => Ok(VPath::new(root.fs, inner)),
                                Some(Err(e)) => Err(format!("Cannot open {name}: {e}")),
                                None => Err(format!("Cannot open {name}: not an archive")),
                            };
                            on_done(app, result);
                        },
                    );
                }),
            );
        }
    }
}

/// A spec from what the user typed: a local path or a server address.
/// Unchanged text keeps the original spec (e.g. a place inside an archive).
/// `Ok(None)`: nothing typed; `Err`: a malformed address.
pub fn parse_location(text: &str, original: Option<&Spec>) -> Result<Option<Spec>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some(o) = original
        && text == o.describe()
    {
        return Ok(Some(o.clone()));
    }
    if let Some(scheme) = oc_provider::scheme_of(text)
        && let Some(provider) = oc_provider::get(&scheme)
    {
        let params = provider.parse_url(text)?;
        let account = provider.account(&params);
        let path = oc_provider::param_str(&params, "path");
        let path = if path.is_empty() { "/".to_owned() } else { path };
        return Ok(Some(Spec::server(account, path)));
    }
    let path = oc_local::absolute(&crate::features::navigation::expand_home(text));
    Ok(Some(Spec::local(path.to_string_lossy().into_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_locations() {
        assert_eq!(parse_location("  ", None), Ok(None));
        let home = oc_local::home_dir();
        let got = parse_location("~/x/../y", None).unwrap().unwrap();
        assert_eq!(got, Spec::local(home.join("y").to_string_lossy().into_owned()));
        // Unchanged text keeps a spec that can't be typed (inside an archive).
        let inside = Spec::archive(Spec::local("/tmp/a.zip"), "docs");
        assert_eq!(parse_location(&inside.describe(), Some(&inside)), Ok(Some(inside.clone())));
    }
}
