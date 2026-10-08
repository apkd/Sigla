mod csharp;
pub(crate) mod preprocess;
mod rust;
pub(crate) use rust::modules as rust_modules;

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
        Language::Markdown | Language::Text => Ok(Facts::default()),
        language => crate::native::extract(source, language),
    }
}
