//! dfapp: installs command-line tools and deploys dotfiles from one or more
//! content repos, driven by a `dfapp.toml` manifest. The CLI and the TUI are
//! thin layers over the modules here.

pub mod cli;
pub mod config;
pub mod config_edit;
pub mod deploy;
pub mod gitops;
pub mod install;
pub mod ops;
pub mod paths;
pub mod plan;
pub mod platform;
pub mod remote;
pub mod schedule;
pub mod selection;
pub mod state;
pub mod tui;
pub mod util;
pub mod version;
