use std::fmt::{Debug, Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[cfg(feature = "blockchain")]
#[doc(hidden)]
pub use alloy_primitives::{Address, B256 as H256, U256};

#[cfg(not(feature = "blockchain"))]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Address(pub [u8; 20]);
#[cfg(not(feature = "blockchain"))]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct H256(pub [u8; 32]);
#[cfg(not(feature = "blockchain"))]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct U256(pub [u64; 4]);

#[cfg(not(feature = "blockchain"))]
macro_rules! byte_scalar {
    ($ty:ty, $len:literal) => {
        impl Deref for $ty {
            type Target = [u8];
            fn deref(&self) -> &Self::Target { &self.0 }
        }
        impl Debug for $ty {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                Display::fmt(self, f)
            }
        }
        impl Display for $ty {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                write!(f, "0x{}", hex::encode(&self.0))
            }
        }
        impl FromStr for $ty {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let bytes = decode_scalar(value, $len)?;
                let mut result = [0u8; $len];
                result.copy_from_slice(&bytes);
                Ok(Self(result))
            }
        }
        impl Serialize for $ty {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where S: Serializer {
                self.to_string().serialize(serializer)
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where D: Deserializer<'de> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(D::Error::custom)
            }
        }
    };
}

#[cfg(not(feature = "blockchain"))]
byte_scalar!(Address, 20);
#[cfg(not(feature = "blockchain"))]
byte_scalar!(H256, 32);

#[cfg(not(feature = "blockchain"))]
impl Deref for U256 {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        // A u64 array has a contiguous byte representation for read-only viewing.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u8>(), 32) }
    }
}
#[cfg(not(feature = "blockchain"))]
impl Debug for U256 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { Display::fmt(self, f) }
}
#[cfg(not(feature = "blockchain"))]
impl Display for U256 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { write!(f, "0x{}", hex::encode(&**self)) }
}
#[cfg(not(feature = "blockchain"))]
impl FromStr for U256 {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = decode_scalar(value, 32)?;
        let mut words = [0u64; 4];
        for (index, chunk) in bytes.chunks_exact(8).enumerate() {
            words[index] = u64::from_be_bytes(chunk.try_into().expect("eight byte chunk"));
        }
        Ok(Self(words))
    }
}
#[cfg(not(feature = "blockchain"))]
impl Serialize for U256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer { self.to_string().serialize(serializer) }
}
#[cfg(not(feature = "blockchain"))]
impl<'de> Deserialize<'de> for U256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

fn decode_scalar(value: &str, digits: usize) -> Result<Vec<u8>, String> {
    if value.len() != digits * 2 + 2 || !value.starts_with("0x") {
        return Err(format!("expected 0x followed by exactly {} hexadecimal digits", digits * 2));
    }
    hex::decode(&value[2..]).map_err(|error| error.to_string())
}

fn parse_blockchain_scalar<E: Error, T: FromStr>(value: &str, digits: usize) -> Result<T, E>
where T::Err: std::fmt::Display {
    if value.len() != digits + 2 || !value.starts_with("0x") {
        return Err(E::custom(format!("expected 0x followed by exactly {digits} hexadecimal digits")));
    }
    T::from_str(value).map_err(|error| E::custom(error.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockchainAddress(pub Address);
impl Debug for BlockchainAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { Debug::fmt(&self.0, f) }
}
#[allow(non_snake_case)]
pub mod WithBlockchainAddress {
    use super::*;
    pub fn serialize<S>(this: &Address, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer { format!("{this:?}").serialize(serializer) }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
    where D: Deserializer<'de> {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, Address>(&value, 40)
    }
}
impl Serialize for BlockchainAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer { format!("{:?}", self.0).serialize(serializer) }
}
impl<'de> Deserialize<'de> for BlockchainAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, Address>(&value, 40).map(Self)
    }
}
impl From<Address> for BlockchainAddress { fn from(value: Address) -> Self { Self(value) } }
impl From<BlockchainAddress> for Address { fn from(value: BlockchainAddress) -> Self { value.0 } }
impl Deref for BlockchainAddress {
    type Target = Address;
    fn deref(&self) -> &Self::Target { &self.0 }
}
impl DerefMut for BlockchainAddress { fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 } }

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockchainTransactionHash(pub H256);
impl Debug for BlockchainTransactionHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result { Debug::fmt(&self.0, f) }
}
#[allow(non_snake_case)]
pub mod WithBlockchainTransactionHash {
    use super::*;
    pub fn serialize<S>(this: &H256, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer { format!("{this:?}").serialize(serializer) }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<H256, D::Error>
    where D: Deserializer<'de> {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, H256>(&value, 64)
    }
}
impl Serialize for BlockchainTransactionHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer { format!("{:?}", self.0).serialize(serializer) }
}
impl<'de> Deserialize<'de> for BlockchainTransactionHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        let value = String::deserialize(deserializer)?;
        parse_blockchain_scalar::<D::Error, H256>(&value, 64).map(Self)
    }
}
impl From<H256> for BlockchainTransactionHash { fn from(value: H256) -> Self { Self(value) } }
impl From<BlockchainTransactionHash> for H256 { fn from(value: BlockchainTransactionHash) -> Self { value.0 } }
impl Deref for BlockchainTransactionHash {
    type Target = H256;
    fn deref(&self) -> &Self::Target { &self.0 }
}
impl DerefMut for BlockchainTransactionHash { fn deref_mut(&mut self) -> &mut Self::Target { &mut self.0 } }
