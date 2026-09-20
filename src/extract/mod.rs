mod csharp;
mod preprocess;
mod rust;

use crate::model::{Facts, Language};

pub fn extract(
    source: &str,
    language: Language,
    defines: &[String],
    edition: &str,
) -> anyhow::Result<Facts> {
    match language {
        Language::CSharp => csharp::extract(source, defines),
        Language::Rust => rust::extract(source, edition),
    }
}
