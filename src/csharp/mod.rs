//! C# declaration facts and demand-driven semantic analysis.
pub(crate) mod bind;
mod cache;
#[cfg(test)]
mod cache_tests;
pub(crate) mod catalog;
mod inference;
pub(crate) mod lower;
#[cfg(test)]
mod oracle;
pub mod syntax;
pub mod types;
