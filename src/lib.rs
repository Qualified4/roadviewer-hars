pub mod auth;
pub mod error;
pub mod ha;
pub mod http;
pub mod logs;
pub mod media;
pub mod migration;
pub mod native_video;
pub mod store;
pub mod telemetry;
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
