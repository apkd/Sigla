use crate::model::Facts;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct FileData {
    pub source: String,
    pub facts: Facts,
    pub assembly: Option<String>,
}
