#![forbid(unsafe_code)]

pub mod api;
pub mod config;
pub mod engine;
pub mod store;

/// CalVer version string: YYYY.MM.MICRO+git_hash
pub const fn version_string() -> &'static str {
    concat!(env!("CARGO_PKG_VERSION"), "+", env!("BUILD_GIT_HASH"),)
}
