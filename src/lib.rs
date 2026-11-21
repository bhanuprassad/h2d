pub mod cli;
pub mod config;
pub mod doctor;
pub mod error;
pub mod job;
pub mod mcp;
pub mod mqtt;
pub mod ops;
pub mod slice;
pub mod status;
pub mod threemf;
pub mod upload;

use serde::Serialize;
use serde_json::{Value, json};

use crate::error::Result;

pub fn with_ok(value: impl Serialize, ok: bool) -> Result<Value> {
    let mut wrapped = serde_json::to_value(value)?;
    if let Some(object) = wrapped.as_object_mut() {
        object.insert("ok".to_string(), json!(ok));
    }
    Ok(wrapped)
}
