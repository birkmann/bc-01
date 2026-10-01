//! Process-wide plumbing shared by every native crate: configuration, path
//! safety and the event bus.

pub mod config;
pub mod events;
pub mod paths;

pub use config::Config;
pub use events::EventBus;
