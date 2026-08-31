use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const MAX_PROTOCOL_VERSIONS: usize = 16;
pub const MAX_CONTACTS: usize = 32;
pub const MAX_HELD_KEYS: usize = 32;
pub const MAX_HELD_BUTTONS: usize = 16;
pub const MAX_MODIFIERS: usize = 8;
pub const MAX_CAPABILITIES: usize = 5;
pub const MAX_POINTER_UNITS: usize = 2;
pub const MAX_DISCOVERY_CANDIDATES: usize = 16;
pub const MAX_STRING_BYTES: usize = 255;
pub const MAX_OPTIONAL_FIELDS: usize = 16;
pub const MAX_OPTIONAL_FIELD_BYTES: usize = 255;
pub const MAX_OPTIONAL_BYTES: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> BoundedVec<T, N> {
    pub(crate) fn try_from_vec(values: Vec<T>, name: &'static str) -> Result<Self, BoundError> {
        if values.len() > N {
            return Err(BoundError::Collection {
                name,
                actual: values.len(),
                maximum: N,
            });
        }
        Ok(Self(values))
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.0
    }

    pub(crate) fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T: Serialize, const N: usize> Serialize for BoundedVec<T, N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedVec<T, N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVecVisitor<T, const N: usize>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const N: usize> de::Visitor<'de> for BoundedVecVisitor<T, N> {
            type Value = BoundedVec<T, N>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a sequence with at most {N} entries")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                if let Some(length) = sequence.size_hint()
                    && length > N
                {
                    return Err(de::Error::invalid_length(length, &self));
                }

                // Never trust a wire-provided size hint as an allocation request.
                let capacity = sequence.size_hint().unwrap_or(0).min(N);
                let mut values = Vec::with_capacity(capacity);
                while let Some(value) = sequence.next_element()? {
                    if values.len() == N {
                        return Err(de::Error::invalid_length(N + 1, &self));
                    }
                    values.push(value);
                }
                Ok(BoundedVec(values))
            }
        }

        deserializer.deserialize_seq(BoundedVecVisitor(std::marker::PhantomData))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BoundedString<const N: usize>(String);

impl<const N: usize> BoundedString<N> {
    pub(crate) fn try_from_string(value: String, name: &'static str) -> Result<Self, BoundError> {
        if value.len() > N {
            return Err(BoundError::String {
                name,
                actual: value.len(),
                maximum: N,
            });
        }
        Ok(Self(value))
    }

    pub(crate) fn into_string(self) -> String {
        self.0
    }
}

impl<const N: usize> Serialize for BoundedString<N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedString<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedStringVisitor<const N: usize>;

        impl<'de, const N: usize> de::Visitor<'de> for BoundedStringVisitor<N> {
            type Value = BoundedString<N>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a UTF-8 string no longer than {N} bytes")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() > N {
                    return Err(E::invalid_length(value.len(), &self));
                }
                Ok(BoundedString(value.to_owned()))
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(value)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.len() > N {
                    return Err(E::invalid_length(value.len(), &self));
                }
                Ok(BoundedString(value))
            }
        }

        // Both codecs use their borrowed slice decoders, so the length check runs
        // before this visitor allocates the owned String.
        deserializer.deserialize_str(BoundedStringVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundError {
    Collection {
        name: &'static str,
        actual: usize,
        maximum: usize,
    },
    String {
        name: &'static str,
        actual: usize,
        maximum: usize,
    },
    Duplicate(&'static str),
    Invalid(&'static str),
}

impl fmt::Display for BoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collection {
                name,
                actual,
                maximum,
            } => write!(
                formatter,
                "{name} has {actual} entries, exceeding the wire limit of {maximum}"
            ),
            Self::String {
                name,
                actual,
                maximum,
            } => write!(
                formatter,
                "{name} has {actual} bytes, exceeding the wire limit of {maximum}"
            ),
            Self::Duplicate(name) => write!(formatter, "{name} contains a duplicate identifier"),
            Self::Invalid(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for BoundError {}
