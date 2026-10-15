use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

#[doc(hidden)]
pub use alloy_primitives::{Address, B256 as H256, U256};
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

fn parse_blockchain_scalar<E: Error, T: FromStr>(
    value: &str,
    prefix: &'static str,
    digits: usize,
) -> Result<T, E>
where
    T::Err: std::fmt::Display,
{
    let bytes = value.as_bytes();
    if bytes.len() != prefix.len() + digits
        || !value.starts_with(prefix)
        || !bytes[prefix.len()..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(E::custom(format!(
            "expected {prefix} followed by exactly {digits} hexadecimal digits"
        )));
    }

    T::from_str(value).map_err(|error| E::custom(error.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockchainAddress(pub Address);
impl Debug for BlockchainAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
#[allow(non_snake_case)]
pub mod WithBlockchainAddress {
    use super::*;

    pub fn serialize<S>(this: &Address, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{this:?}").serialize(serializer)
    }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, Address>(&value, "0x", 40)
    }
}
impl Serialize for BlockchainAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", self.0).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for BlockchainAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, Address>(&value, "0x", 40).map(BlockchainAddress)
    }
}

impl From<Address> for BlockchainAddress {
    fn from(address: Address) -> Self {
        BlockchainAddress(address)
    }
}
impl From<BlockchainAddress> for Address {
    fn from(val: BlockchainAddress) -> Self {
        val.0
    }
}

impl Deref for BlockchainAddress {
    type Target = Address;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for BlockchainAddress {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockchainTransactionHash(pub H256);
impl Debug for BlockchainTransactionHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
#[allow(non_snake_case)]
pub mod WithBlockchainTransactionHash {
    use super::*;

    pub fn serialize<S>(this: &H256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{this:?}").serialize(serializer)
    }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<H256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, H256>(&value, "0x", 64)
    }
}
impl Serialize for BlockchainTransactionHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        format!("{:?}", self.0).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for BlockchainTransactionHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, H256>(&value, "0x", 64).map(BlockchainTransactionHash)
    }
}

impl From<H256> for BlockchainTransactionHash {
    fn from(hash: H256) -> Self {
        BlockchainTransactionHash(hash)
    }
}
impl From<BlockchainTransactionHash> for H256 {
    fn from(val: BlockchainTransactionHash) -> Self {
        val.0
    }
}

impl Deref for BlockchainTransactionHash {
    type Target = H256;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for BlockchainTransactionHash {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod scalar_decode_tests {
    use super::*;
    use serde::de::value::{Error as DecodeError, StrDeserializer};

    #[test]
    fn scalar_entry_points_decode_hex_values_and_reject_malformed_input() {
        let address = format!("0x{}", "aB".repeat(20));
        let hash = format!("0x{}", "aB".repeat(32));
        let decode = |value| StrDeserializer::<DecodeError>::new(value);
        assert_eq!(
            WithBlockchainAddress::deserialize(decode(&address)).unwrap(),
            Address::from_str(&address).unwrap()
        );
        assert_eq!(
            BlockchainAddress::deserialize(decode(&address)).unwrap().0,
            Address::from_str(&address).unwrap()
        );
        assert_eq!(
            WithBlockchainTransactionHash::deserialize(decode(&hash)).unwrap(),
            H256::from_str(&hash).unwrap()
        );
        assert_eq!(
            BlockchainTransactionHash::deserialize(decode(&hash))
                .unwrap()
                .0,
            H256::from_str(&hash).unwrap()
        );
        for invalid in [
            "",
            "0x",
            "é",
            "0x🦀",
            &"0".repeat(40),
            &format!("0x{}", "g".repeat(64)),
            &format!("0X{}", "a".repeat(40)),
            &format!("0x{}", "a".repeat(65)),
        ] {
            assert!(WithBlockchainAddress::deserialize(decode(invalid)).is_err());
            assert!(BlockchainAddress::deserialize(decode(invalid)).is_err());
            assert!(WithBlockchainTransactionHash::deserialize(decode(invalid)).is_err());
            assert!(BlockchainTransactionHash::deserialize(decode(invalid)).is_err());
        }
    }
}
