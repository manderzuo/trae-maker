use std::fmt;

use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize, Serializer};

/// A non-negative credits amount stored as millionths of one credit.
///
/// Parsing and serialization are decimal-string based so no floating-point
/// rounding can enter quota checks or settlement records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CreditAmount(i64);

impl CreditAmount {
    pub fn parse(value: &str, unit: &str) -> Result<Self, String> {
        if unit != "credits" {
            return Err("unit must be credits".into());
        }

        let value = value.trim();
        if value.is_empty() || value.len() > 64 || value.starts_with(['-', '+']) {
            return Err("amount must be a non-negative decimal credits string".into());
        }

        let (whole, fraction) = match value.split_once('.') {
            Some((whole, fraction)) if !fraction.contains('.') && !fraction.is_empty() => {
                (whole, fraction)
            }
            Some(_) => return Err("amount must have one decimal point and digits after it".into()),
            None => (value, ""),
        };
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 6
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("amount must contain at most six decimal places".into());
        }

        let whole = whole
            .parse::<i64>()
            .map_err(|_| "amount exceeds the supported credits range".to_string())?;
        let whole_microcredits = whole
            .checked_mul(1_000_000)
            .ok_or_else(|| "amount exceeds the supported credits range".to_string())?;
        let mut fractional_microcredits = 0_i64;
        for digit in fraction.bytes() {
            fractional_microcredits = fractional_microcredits * 10 + i64::from(digit - b'0');
        }
        for _ in fraction.len()..6 {
            fractional_microcredits *= 10;
        }
        let microcredits = whole_microcredits
            .checked_add(fractional_microcredits)
            .ok_or_else(|| "amount exceeds the supported credits range".to_string())?;

        Ok(Self(microcredits))
    }

    pub fn try_from_microcredits(value: i64) -> Option<Self> {
        (value >= 0).then_some(Self(value))
    }

    pub const fn as_microcredits(self) -> i64 {
        self.0
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        let value = self.0.checked_add(other.0)?;
        Self::try_from_microcredits(value)
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        let value = self.0.checked_sub(other.0)?;
        Self::try_from_microcredits(value)
    }
}

impl fmt::Display for CreditAmount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:06}", self.0 / 1_000_000, self.0 % 1_000_000)
    }
}

impl Serialize for CreditAmount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CreditAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value, "credits").map_err(D::Error::custom)
    }
}
