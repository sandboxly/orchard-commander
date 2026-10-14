//! Features: each module registers its commands, side views and hooks from
//! one `register` function. Adding a feature = a new module here plus one
//! line in [`ALL`] (see docs/ARCHITECTURE.md).

pub mod about;
pub mod bookmarks;
pub mod editor;
pub mod fileops;
pub mod info;
pub mod navigation;
pub mod preview;
pub mod rename;
pub mod search;
pub mod settings;
pub mod terminal;
pub mod window;

use crate::commands::Registry;

/// Every feature's registration, in the order their commands should appear
/// (menus and the palette keep registration order within a section).
/// `connections` comes before `window` so its "⌘K Connect" sits left of
/// "⌘P Commands" at the right end of the F-key bar, as in 1.x.
pub const ALL: &[fn(&mut Registry)] = &[
    crate::main_window::register,
    crate::connections::register,
    settings::register,
    window::register,
    preview::register,
    fileops::register,
    info::register,
    navigation::register,
    editor::register,
    terminal::register,
    search::register,
    rename::register,
    bookmarks::register,
    about::register,
];
