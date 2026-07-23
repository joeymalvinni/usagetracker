use serde::{Deserialize, Serialize};

use crate::providers::{ProviderError, ProviderErrorKind};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum NumberLike {
    Integer(i64),
    Float(f64),
    String(String),
}

impl NumberLike {
    pub(super) fn value(&self) -> Option<f64> {
        match self {
            Self::Integer(value) => Some(*value as f64),
            Self::Float(value) => value.is_finite().then_some(*value),
            Self::String(value) => value.parse().ok().filter(|value: &f64| value.is_finite()),
        }
    }

    pub(super) fn nonnegative_value(&self, field: &str) -> Result<f64, ProviderError> {
        let value = match self {
            Self::Integer(value) => *value as f64,
            Self::Float(value) => *value,
            Self::String(value) => value.parse().map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Parse,
                    format!("Cursor usage event {field} was not numeric"),
                )
            })?,
        };
        if !value.is_finite() || value < 0.0 {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Cursor usage event {field} was invalid"),
            ));
        }
        Ok(value)
    }

    pub(super) fn nonnegative_integer(&self, field: &str) -> Result<u64, ProviderError> {
        let value = self.nonnegative_value(field)?;
        if value.fract() != 0.0 || value > u64::MAX as f64 {
            return Err(ProviderError::new(
                ProviderErrorKind::Parse,
                format!("Cursor usage event {field} exceeded the supported integer range"),
            ));
        }
        Ok(value as u64)
    }
}
