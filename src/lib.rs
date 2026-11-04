//! dfapp: installs command-line tools and deploys dotfiles from one or more
//! content repos, driven by a `dfapp.toml` manifest. The CLI and the TUI are
//! thin layers over the modules here.

pub mod config;
pub mod install;
pub mod paths;
pub mod platform;
pub mod remote;
pub mod selection;
pub mod state;
pub mod util;
pub mod version;
