//! Shared library behind the sidebar binaries: the sidebar TUI
//! (`herdr-sidebar`, hosting BOTH views — file explorer and source
//! control — plus the `--preview` file viewer) and the windowless ensure
//! sidecar (`herdr-sidebar-ensure`).

// The TUI's own modules name the library `herdr_sidebar::…`, as they did when
// they lived in the binary.
extern crate self as herdr_sidebar;

pub mod actions;
pub mod ansi;
pub mod branch_ui;
pub mod cli;
pub mod diffview;
pub mod editor;
pub mod ensure;
pub mod find;
pub mod fontsetup;
pub mod git;
pub mod gitdeco;
pub mod host;
pub mod icons;
pub mod ipc;
pub mod launch;
pub mod magpie;
pub mod rundir;
pub mod serve;
pub mod site;
pub mod snooze;
pub mod state;
pub mod suggest;
pub mod syntax;
pub mod system_metrics;
pub mod takeover;
pub mod tree;
pub mod ui;
pub mod updates;
pub mod upload;
pub mod usage;
pub mod viewer;
pub mod wrap;
