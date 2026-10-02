mod acquisition;
mod cache_migration;
pub mod config;
pub mod csharp;
mod diagnostics;
pub mod discovery;
mod documents;
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
mod summary;
pub mod unity;
pub mod watch;
pub mod workspace;
