//! Batch renaming with name patterns (multi-rename, ⌘M / Ctrl+M).
//!
//! A [`Rule`] turns each file's old name into a new one in three steps:
//!
//! 1. the **masks** — name and extension — with placeholders in brackets:
//!    `[N]` name, `[E]` extension, ranges like `[N2-5] [N2,5] [N2-] [N-8,5]
//!    [N-8-5] [N2--5] [N-5-]`, `[2-5]` over the whole name, `[P]` parent and
//!    `[G]` grandparent folder, `[C]` counter (`[C10+5:3]`: start 10, step 5,
//!    3 digits), `[d] [t]` date and time of last modification, `[Y] [M] [D]
//!    [h] [m] [s]`, case switches `[U] [L] [F] [n]` that apply to what
//!    follows, `[[] []]` for brackets and `[=field]` for file metadata (see
//!    [`metadata`]);
//! 2. **search & replace** on the result — several pairs separated by `|`,
//!    plain or RegEx (`$1` / `\1` for groups), case-insensitive unless asked;
//! 3. a **case conversion** of the whole name.
//!
//! [`plan`] checks the outcome (invalid names, duplicates, existing files) and
//! [`execute`] renames safely: swaps and case-only changes go through
//! temporary names, so nothing is ever overwritten. Everything works through
//! the generic [`oc_vfs::FileSystem`] interface. Port of `rename.py` and
//! `metadata.py`; the UI-free half of the Multi-Rename dialog.

mod mask;
pub mod metadata;
mod plan;
mod rule;
mod search;

pub use mask::{MaskError, NameError, Renamer, expand, needs_metadata, new_name};
pub use plan::{
    Change, Done, ExistingNames, Plan, Renamed, execute, existing_names, name_problem, plan, plan_one, plan_with,
    rename_supported, renamed_locations, undo,
};
pub use rule::{CASES, Case, Item, Rule, item_for, split_name};
