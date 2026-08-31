use serde::{Deserialize, Serialize};

use super::{Codec, WireError};

pub(crate) fn encode<T: Serialize>(codec: Codec, value: &T) -> Result<Vec<u8>, WireError> {
    match codec {
        Codec::Postcard => postcard::to_allocvec(value).map_err(|error| WireError::Codec {
            codec,
            detail: error.to_string(),
        }),
        Codec::Bincode => bincode::serde::encode_to_vec(value, bincode_config()).map_err(|error| {
            WireError::Codec {
                codec,
                detail: error.to_string(),
            }
        }),
    }
}

pub(crate) fn decode<'de, T: Deserialize<'de>>(
    codec: Codec,
    payload: &'de [u8],
) -> Result<T, WireError> {
    match codec {
        Codec::Postcard => {
            let (value, remainder) =
                postcard::take_from_bytes(payload).map_err(|error| WireError::Codec {
                    codec,
                    detail: error.to_string(),
                })?;
            if !remainder.is_empty() {
                return Err(WireError::TrailingPayload);
            }
            Ok(value)
        }
        Codec::Bincode => {
            // The borrowed decoder lets bounded string visitors inspect lengths
            // before allocating owned strings.
            let (value, consumed) =
                bincode::serde::borrow_decode_from_slice(payload, bincode_config()).map_err(
                    |error| WireError::Codec {
                        codec,
                        detail: error.to_string(),
                    },
                )?;
            if consumed != payload.len() {
                return Err(WireError::TrailingPayload);
            }
            Ok(value)
        }
    }
}

fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_variable_int_encoding()
        .with_little_endian()
        .with_limit::<65_536>()
}
