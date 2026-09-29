mod acquisition;
pub mod config;
pub mod csharp;
pub mod discovery;
pub mod extract;
mod memory;
pub mod metadata;
mod minify;
pub mod model;
mod msbuild;
mod navigation;
mod process;
mod sandbox;
mod upstream;

pub fn shutdown() {
    process::shutdown();
    acquisition::shutdown();
}
pub mod query;
mod render;
pub mod repository;
pub mod search;
mod selection;
pub mod service;
pub mod signature;
pub mod store;
pub mod unity;
pub mod watch;
pub mod workspace;
