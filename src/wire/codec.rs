use serde::{Deserialize, Serialize};

use super::WireError;

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    postcard::to_allocvec(value).map_err(|error| WireError::Codec(error.to_string()))
}

pub(crate) fn decode<'de, T: Deserialize<'de>>(payload: &'de [u8]) -> Result<T, WireError> {
    let (value, remainder) =
        postcard::take_from_bytes(payload).map_err(|error| WireError::Codec(error.to_string()))?;
    if !remainder.is_empty() {
        return Err(WireError::TrailingPayload);
    }
    Ok(value)
}
