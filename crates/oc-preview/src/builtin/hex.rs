//! Any file: facts and a hex dump of the beginning. The last-resort previewer.

use oc_vfs::human_size;

use crate::api::{LoadContext, Loaded, PreviewError, Previewer, Score, FileInfo, Access};
use crate::content::{self, HEXDUMP_BYTES};
use crate::data::{HexData, PreviewData};
use crate::source::PreviewSource;

pub struct HexPreviewer;

impl Previewer for HexPreviewer {
    fn id(&self) -> &str {
        "hex"
    }

    fn title(&self) -> &str {
        "Hex"
    }

    fn access(&self) -> Access {
        Access::Head
    }

    fn reload_on_theme_change(&self) -> bool {
        false
    }

    fn score(&self, f: &FileInfo) -> i32 {
        if f.is_dir { Score::NO } else { Score::FALLBACK }
    }

    fn load(&self, src: &PreviewSource, _ctx: &LoadContext) -> Result<Loaded, PreviewError> {
        if src.size() == 0 {
            return Ok(Loaded::new(PreviewData::Hex(HexData::default()), "0 B"));
        }
        // Local files: permissions and link targets too.
        let facts = src
            .direct_path()
            .and_then(|p| content::describe(p).ok())
            .unwrap_or_else(|| content::describe_entry(src.entry(), &src.location()));
        let bytes = src.head(HEXDUMP_BYTES)?;
        let data = HexData { facts, offset: 0, bytes, size: src.size() };
        Ok(Loaded::new(PreviewData::Hex(data), format!("{} · binary", human_size(src.size()))))
    }
}
