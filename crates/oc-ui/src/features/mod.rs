//! Features: each module registers its commands, side views and hooks from
//! one `register` function. Adding a feature = a new module here plus one
//! line in [`ALL`] (see docs/ARCHITECTURE.md).

pub mod editor;
pub mod fileops;
pub mod navigation;
pub mod preview;
pub mod rename;
pub mod search;
pub mod terminal;
pub mod window;

use crate::commands::Registry;

/// Every feature's registration, in the order their commands should appear
/// (menus and the palette keep registration order within a section).
pub const ALL: &[fn(&mut Registry)] = &[
    crate::main_window::register,
    window::register,
    preview::register,
    fileops::register,
    navigation::register,
    editor::register,
    terminal::register,
    search::register,
    rename::register,
];
