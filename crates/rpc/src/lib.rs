//! The dispatcher: one thread that owns all protocol state. FreeRDP callbacks only put work on
//! a queue (plan §2.3). Media is behind the `media::MediaEngine` trait; its callbacks come back to
//! this thread as queued work.

pub mod callalert;
pub mod config;
pub mod core;
pub mod devices;
pub mod host;
pub mod media_objects;
pub mod mediadevices;
pub mod notify;
pub mod peerconnection;
pub mod redirector;
pub mod registry;
mod tones;
pub mod video;

pub use crate::core::Core;
pub use config::{Config, CpcMode, SessionInfo};
pub use devices::{Device, DeviceKind, DeviceSource, PactlDevices, SystemDevices};
pub use host::{Channel, ConnId, Host, LogLevel};
