//! Folders and archives: a listing of what's inside.

use oc_vfs::{ListOptions, human_size};

use crate::api::{Access, FileInfo, LoadContext, Loaded, PreviewError, Previewer, Score};
use crate::content::{MAX_LISTING_ENTRIES, is_archive};
use crate::data::{ListingData, PreviewData};
use crate::source::PreviewSource;

use super::MAX_FETCH_BYTES;

pub struct ListingPreviewer;

impl Previewer for ListingPreviewer {
    fn id(&self) -> &str {
        "listing"
    }

    fn title(&self) -> &str {
        "Contents"
    }

    fn access(&self) -> Access {
        Access::Metadata
    }

    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        if f.is_dir || is_archive(&f.name) { Score::FORMAT } else { Score::NO }
    }

    fn load(&self, src: &PreviewSource, ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        let target = src.target();
        let opts = ListOptions { show_hidden: ctx.show_hidden, ..ListOptions::default() };
        if src.entry().is_dir {
            let listing = ListingData::new(target.list_dir(opts)?, MAX_LISTING_ENTRIES);
            return Ok(Loaded::new(PreviewData::Listing(listing), format!("folder · {}", target.fs.type_name())));
        }
        // The context's opener (tests, special hosts) or the process-wide one
        // the app registered with oc_vfs.
        if ctx.open_archive.is_none() && oc_vfs::archive_opener().is_none() {
            return Err(PreviewError::cannot("Archive listings are not available"));
        }
        // The whole archive has to be fetched to list it: refuse big ones up
        // front. The opener fetches it itself; a copy made here would be a
        // second download nobody uses.
        src.check_fetch_size(Some(MAX_FETCH_BYTES))?;
        let opened = match ctx.open_archive.as_ref() {
            Some(open_archive) => open_archive(target),
            None => oc_vfs::open_archive(target)
                .unwrap_or_else(|| Err(oc_vfs::VfsError::Operation(format!("{} is not a supported archive", target.name())))),
        };
        let root = opened.map_err(|e| PreviewError::ShowMessage(e.to_string()))?;
        let entries = root.list_dir(opts).map_err(|e| PreviewError::ShowMessage(e.to_string()))?;
        let listing = ListingData::new(entries, MAX_LISTING_ENTRIES);
        let info = format!("{} · {}", root.fs.type_name(), human_size(src.size()));
        Ok(Loaded::new(PreviewData::Listing(listing), info))
    }
}
