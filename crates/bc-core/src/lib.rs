//! Process-wide plumbing shared by every native crate: configuration, path
//! safety and the event bus.

pub mod audio;
pub mod config;
pub mod events;
pub mod paths;
pub mod slug;

pub use config::Config;
pub use events::EventBus;
