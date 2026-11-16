pub mod annotations;
pub mod archive;
pub mod auth;
pub mod body_limit;
pub mod convert;
pub mod error;
#[cfg(feature = "pure-remux")]
pub mod experimental;
pub mod fault;
pub mod ha;
pub mod http;
pub mod http_storage;
pub mod jobs;
pub mod listing;
pub mod logs;
pub mod media;
pub mod migration;
pub mod native_video;
pub mod network;
pub mod parity;
pub mod profile;
pub mod recording_update;
pub mod replay;
pub mod storage;
pub mod store;
pub mod telemetry;
pub mod thumbnail;
pub mod track;
pub mod transfer;
pub mod upload;

// Generated schemas reference each other through crate-level modules.
#[allow(clippy::all, clippy::pedantic, unused_parens)]
pub mod log_capnp {
    include!(concat!(env!("OUT_DIR"), "/log_capnp.rs"));
}
#[allow(clippy::all, clippy::pedantic, unused_parens)]
pub mod car_capnp {
    include!(concat!(env!("OUT_DIR"), "/car_capnp.rs"));
}
#[allow(clippy::all, clippy::pedantic, unused_parens)]
pub mod custom_capnp {
    include!(concat!(env!("OUT_DIR"), "/custom_capnp.rs"));
}
#[allow(clippy::all, clippy::pedantic, unused_parens)]
pub mod deprecated_capnp {
    include!(concat!(env!("OUT_DIR"), "/deprecated_capnp.rs"));
}
