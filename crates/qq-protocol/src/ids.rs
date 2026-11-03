use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

const ID_BYTES: usize = 16;
const ID_HEX_BYTES: usize = ID_BYTES * 2;

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; ID_BYTES]);

        impl $name {
            pub fn generate() -> Result<Self, IdError> {
                let mut bytes = [0_u8; ID_BYTES];
                getrandom::fill(&mut bytes).map_err(|_| IdError::RandomnessUnavailable)?;
                Ok(Self(bytes))
            }

            #[must_use]
            pub const fn from_bytes(bytes: [u8; ID_BYTES]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn as_bytes(self) -> [u8; ID_BYTES] {
                self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_id(value).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_id(formatter, &self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.to_string())
                    .finish()
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }
    };
}

identifier!(StoreId);
identifier!(WorkspaceId);
identifier!(SessionId);
identifier!(RunId);
identifier!(MessageId);
identifier!(ToolCallId);
identifier!(CommandId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    InvalidLength,
    InvalidCharacter,
    RandomnessUnavailable,
}

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLength => "identifier must contain 32 hexadecimal characters",
            Self::InvalidCharacter => "identifier must use lowercase hexadecimal characters",
            Self::RandomnessUnavailable => "secure randomness is unavailable",
        })
    }
}

impl std::error::Error for IdError {}

fn parse_id(value: &str) -> Result<[u8; ID_BYTES], IdError> {
    if value.len() != ID_HEX_BYTES {
        return Err(IdError::InvalidLength);
    }

    let mut output = [0_u8; ID_BYTES];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(output)
}

const fn hex_nibble(byte: u8) -> Result<u8, IdError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(IdError::InvalidCharacter),
    }
}

fn write_id(formatter: &mut fmt::Formatter<'_>, bytes: &[u8; ID_BYTES]) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = [0_u8; ID_HEX_BYTES];
    for (index, byte) in bytes.iter().copied().enumerate() {
        encoded[index * 2] = HEX[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
    }
    let encoded = std::str::from_utf8(&encoded).map_err(|_| fmt::Error)?;
    formatter.write_str(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_round_trip_through_the_wire_format() {
        let id = SessionId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);

        assert_eq!(id.to_string(), "00112233445566778899aabbccddeeff");
        assert_eq!(id.to_string().parse::<SessionId>(), Ok(id));
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"00112233445566778899aabbccddeeff\""
        );
        assert_eq!(
            serde_json::from_str::<SessionId>("\"00112233445566778899aabbccddeeff\"").unwrap(),
            id
        );
    }

    #[test]
    fn identifiers_reject_noncanonical_input() {
        assert_eq!("abc".parse::<RunId>(), Err(IdError::InvalidLength));
        assert_eq!(
            "00112233445566778899AABBCCDDEEFF".parse::<RunId>(),
            Err(IdError::InvalidCharacter)
        );
    }
}
