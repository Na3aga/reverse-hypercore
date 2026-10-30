//! Identifiers and small shared enums.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub type AssetId = u32;
pub type Oid = u64;

/// 20-byte account address: the Ethereum address of the account's secp256k1
/// key (see [`crate::action::address_of`]), shared by core and EVM.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Address(pub [u8; 20]);

impl Address {
    /// Reserved system accounts are derived from a label so they can never
    /// collide with a key-derived address in practice.
    pub fn system(label: &str) -> Address {
        let h = blake3::hash(format!("hc-system:{label}").as_bytes());
        let mut a = [0u8; 20];
        a.copy_from_slice(&h.as_bytes()[..20]);
        Address(a)
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex_encode(&self.0))
    }

    pub fn from_hex(s: &str) -> Option<Address> {
        let b = hex_decode(s.strip_prefix("0x").unwrap_or(s))?;
        Some(Address(b.try_into().ok()?))
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&self.to_hex())
        } else {
            self.0.serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Address, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            Address::from_hex(&s).ok_or_else(|| de::Error::custom("invalid address"))
        } else {
            Ok(Address(<[u8; 20]>::deserialize(d)?))
        }
    }
}

pub fn hex_encode(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(b.len() * 2);
    for x in b {
        out.push(DIGITS[(x >> 4) as usize]);
        out.push(DIGITS[(x & 15) as usize]);
    }
    String::from_utf8(out).expect("hex digits are ASCII")
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Fixed-size byte arrays that serialize as `0x` hex in JSON and raw bytes in
/// the binary encoding.
macro_rules! hex_bytes {
    ($name:ident, $n:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; $n]);

        impl Default for $name {
            fn default() -> Self {
                $name([0u8; $n])
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "0x{}", hex_encode(&self.0))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.serialize_str(&format!("0x{}", hex_encode(&self.0)))
                } else {
                    // serde only derives arrays up to 32 elements, so use bytes.
                    s.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<$name, D::Error> {
                let bytes: Vec<u8> = if d.is_human_readable() {
                    let s = String::deserialize(d)?;
                    hex_decode(s.strip_prefix("0x").unwrap_or(&s))
                        .ok_or_else(|| de::Error::custom("invalid hex"))?
                } else {
                    serde_bytes_vec(d)?
                };
                let arr: [u8; $n] = bytes.try_into().map_err(|_| de::Error::custom("bad length"))?;
                Ok($name(arr))
            }
        }
    };
}

fn serde_bytes_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    struct V;
    impl<'de> de::Visitor<'de> for V {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bytes")
        }
        fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }
        fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }
        fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }
    }
    d.deserialize_bytes(V)
}

hex_bytes!(Sig, 65);
hex_bytes!(Hash32, 32);
hex_bytes!(Cloid, 16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Bid / buy. Serialized as `"B"` in the public API.
    #[serde(rename = "B")]
    Bid,
    /// Ask / sell. Serialized as `"A"` in the public API.
    #[serde(rename = "A")]
    Ask,
}

impl Side {
    pub fn from_is_buy(is_buy: bool) -> Side {
        if is_buy {
            Side::Bid
        } else {
            Side::Ask
        }
    }
    pub fn sign(self) -> i64 {
        match self {
            Side::Bid => 1,
            Side::Ask => -1,
        }
    }
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Time in force, with the public wire names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tif {
    /// Add liquidity only (post-only).
    Alo,
    /// Immediate or cancel.
    Ioc,
    /// Good til canceled.
    Gtc,
}

/// Take-profit or stop-loss, for trigger orders (public wire names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tpsl {
    Tp,
    Sl,
}

/// Terminal and non-terminal order statuses, using the public status strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderStatus {
    Open,
    Filled,
    Canceled,
    MarginCanceled,
    SelfTradeCanceled,
    ReduceOnlyCanceled,
    LiquidatedCanceled,
    TickRejected,
    MinTradeNtlRejected,
    PerpMarginRejected,
    ReduceOnlyRejected,
    BadAloPxRejected,
    IocCancelRejected,
    MarketOrderNoLiquidityRejected,
    OracleRejected,
    /// A trigger order's condition was met; it was sent as a market or limit order.
    Triggered,
    /// A TP/SL was cancelled because its position closed (or its entry order
    /// was cancelled before filling).
    SiblingFilledCanceled,
}
