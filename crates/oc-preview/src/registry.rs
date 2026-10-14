//! Which previewer shows a file: registration, scoring, fallback.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock, RwLock};

use oc_vfs::{VPath, VfsError};

use crate::api::{Candidate, FileInfo, LoadContext, Outcome, PreviewError, Previewer, Score};
use crate::builtin;
use crate::source::PreviewSource;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("A previewer with id '{0}' is already registered")]
    Duplicate(String),
}

/// The registered previewers, in registration order (which breaks score ties).
#[derive(Default)]
pub struct Registry {
    previewers: RwLock<Vec<Arc<dyn Previewer>>>,
}

impl Registry {
    /// An empty registry (tests, embedding).
    pub fn new() -> Self {
        Registry::default()
    }

    /// A registry with the built-in previewers.
    pub fn with_builtins() -> Self {
        let r = Registry::new();
        for p in builtin::builtins() {
            r.register(p, false).expect("built-in ids are unique");
        }
        r
    }

    /// The app-wide registry, with the built-ins; add-ons register here.
    pub fn global() -> &'static Registry {
        static GLOBAL: OnceLock<Registry> = OnceLock::new();
        GLOBAL.get_or_init(Registry::with_builtins)
    }

    /// Add a previewer. Unavailable ones (missing dependency) are skipped. A
    /// different previewer with the same id replaces the old one (keeping its
    /// place) only with `replace`.
    pub fn register(&self, previewer: Arc<dyn Previewer>, replace: bool) -> Result<(), RegistryError> {
        if !previewer.available() {
            log::info!("previewer {} unavailable (missing dependency?)", previewer.id());
            return Ok(());
        }
        let mut list = self.previewers.write().unwrap_or_else(|e| e.into_inner());
        match list.iter().position(|p| p.id() == previewer.id()) {
            Some(_) if !replace => Err(RegistryError::Duplicate(previewer.id().to_string())),
            Some(i) => {
                list[i] = previewer;
                Ok(())
            }
            None => {
                list.push(previewer);
                Ok(())
            }
        }
    }

    pub fn unregister(&self, id: &str) {
        self.previewers.write().unwrap_or_else(|e| e.into_inner()).retain(|p| p.id() != id);
    }

    pub fn previewers(&self) -> Vec<Arc<dyn Previewer>> {
        self.previewers.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Previewer>> {
        self.previewers.read().unwrap_or_else(|e| e.into_inner()).iter().find(|p| p.id() == id).cloned()
    }

    /// Previewers that can show `src`, best first.
    ///
    /// Scoring is cheap by name/size first; the file's first bytes are read
    /// (once) only if a head-sniffing previewer could still win — and only if
    /// `may_read` (reading an online-only cloud file downloads all of it).
    pub fn candidates(&self, src: &PreviewSource, may_read: bool) -> Result<Vec<Candidate>, VfsError> {
        let available = self.previewers();
        let info = src.info(false)?;
        let scored = score_all(&available, &info);
        let best = scored.first().map_or(Score::NO, |c| c.score);
        let sniffers = available.iter().any(|p| p.needs_head());
        if may_read && sniffers && !info.is_dir && info.size > 0 && best < Score::FORMAT {
            return Ok(score_all(&available, &src.info(true)?));
        }
        Ok(scored)
    }

    /// Pick and run previewers for `target` (worker thread).
    ///
    /// Online-only files are fetched first with a time limit (any read, even
    /// sniffing, would block until the cloud app delivers), or not at all
    /// above `ctx.download_limit`. Then the preferred previewer is tried (if
    /// it can show the file), then the others by score: `CannotPreview` falls
    /// through to the next, anything else ends the search and is reported.
    pub fn preview(&self, target: &VPath, ctx: &LoadContext, preferred: Option<&str>) -> Outcome {
        let mut outcome = Outcome::default();
        let src = match PreviewSource::new(target.clone()) {
            Ok(s) => match &ctx.cache_dir {
                Some(dir) => s.with_cache_dir(dir),
                None => s,
            },
            Err(e) => {
                outcome.error = Some(e.into());
                return outcome;
            }
        };
        let entry = src.entry();
        let too_big = entry.is_online_only && ctx.download_limit.is_some_and(|limit| entry.size > limit);
        if entry.is_online_only && !too_big {
            let timeout = (ctx.fetch_timeout)(entry.size);
            match target.fs.fetch_online_only(&target.path, timeout, &*ctx.is_cancelled) {
                Ok(()) => {}
                Err(VfsError::Cancelled) => return outcome, // the pane has moved on
                Err(VfsError::FetchTimeout(_)) => {
                    match self.candidates(&src, false) {
                        Ok(c) => {
                            outcome.candidates = c;
                            outcome.error =
                                Some(PreviewError::CloudNotDelivered { name: src.name().to_string(), waited: timeout });
                        }
                        Err(e) => outcome.error = Some(e.into()),
                    }
                    return outcome;
                }
                Err(e) => {
                    outcome.error = Some(e.into());
                    return outcome;
                }
            }
        }
        match self.candidates(&src, !too_big) {
            Ok(c) => outcome.candidates = c,
            Err(e) => {
                outcome.error = Some(e.into());
                return outcome;
            }
        }
        if too_big {
            // Don't start a big cloud download just because the cursor passed by.
            outcome.error = Some(PreviewError::NeedsDownload { size: entry.size });
            return outcome;
        }
        let mut order: Vec<&Candidate> = outcome.candidates.iter().collect();
        order.sort_by_key(|c| Some(c.previewer.id()) != preferred); // stable: preferred first
        let order: Vec<Arc<dyn Previewer>> = order.into_iter().map(|c| c.previewer.clone()).collect();
        let mut first_failure = None;
        for previewer in order {
            if ctx.cancelled() {
                return outcome;
            }
            let result = catch_unwind(AssertUnwindSafe(|| previewer.load(&src, ctx))).unwrap_or_else(|_| {
                log::error!("previewer {} panicked on {}", previewer.id(), src.name());
                Err(PreviewError::Failed(format!("The {} preview failed", previewer.title())))
            });
            match result {
                Ok(loaded) => {
                    outcome.loaded = Some(loaded);
                    outcome.previewer = Some(previewer);
                    return outcome;
                }
                Err(PreviewError::CannotPreview(msg)) => {
                    first_failure.get_or_insert(msg);
                }
                Err(e) => {
                    outcome.previewer = Some(previewer);
                    outcome.error = Some(e);
                    return outcome;
                }
            }
        }
        outcome.error = Some(PreviewError::CannotPreview(
            first_failure.unwrap_or_else(|| "Can't preview this kind of file".to_string()),
        ));
        outcome
    }
}

/// Positive scores, best first; ties keep registration order. A previewer
/// that panics while scoring is logged and skipped.
fn score_all(available: &[Arc<dyn Previewer>], info: &FileInfo) -> Vec<Candidate> {
    let mut result: Vec<Candidate> = available
        .iter()
        .filter_map(|p| match catch_unwind(AssertUnwindSafe(|| p.score(info))) {
            Ok(score) if score > 0 => Some(Candidate { previewer: p.clone(), score }),
            Ok(_) => None,
            Err(_) => {
                log::error!("previewer {} failed to score {}", p.id(), info.name);
                None
            }
        })
        .collect();
    result.sort_by_key(|c| std::cmp::Reverse(c.score)); // stable: registration order breaks ties
    result
}
