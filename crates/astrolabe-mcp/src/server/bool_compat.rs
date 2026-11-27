//! Tolerate explicit JSON string booleans without truthiness coercions.
//! Attached to request fields, so stdio, shared dispatch and direct serde
//! adapters all use the same rules while their schemas remain boolean.
use serde::de::Error;
use serde::{Deserialize, Deserializer};

pub(crate) fn required<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(value) => Ok(value),
        serde_json::Value::String(value) if value == "true" => Ok(true),
        serde_json::Value::String(value) if value == "false" => Ok(false),
        other => Err(D::Error::custom(format!(
            "expected a JSON boolean or the string \"true\"/\"false\"; got {other}"
        ))),
    }
}

pub(crate) fn optional<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(d)?;
    value
        .map(|value| required(value).map_err(D::Error::custom))
        .transpose()
}
