//! Steam discovery, read-only (Story 4.2.1). `library.rs` builds the installed set with `steamlocate`
//! (spike 0.2.3); compatdata and shadercache enumeration belongs to Story 4.2.2.

pub mod library;
pub mod list;

pub use library::{
    default_steam_roots, downloading_nonempty, plan_gate, InstalledApp, SteamGate, SteamInstall,
};
