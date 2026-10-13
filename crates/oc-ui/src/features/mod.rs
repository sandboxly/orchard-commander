//! Features: each module registers its commands, side views and hooks from
//! one `register` function. Adding a feature = a new module here plus one
//! line in [`ALL`] (see docs/ARCHITECTURE.md).

pub mod fileops;
pub mod navigation;
pub mod preview_stub;
pub mod search;
pub mod window;

use crate::commands::Registry;

/// Every feature's registration, in the order their commands should appear
/// (menus and the palette keep registration order within a section).
pub const ALL: &[fn(&mut Registry)] =
    &[crate::main_window::register, window::register, preview_stub::register, fileops::register, navigation::register, search::register];
