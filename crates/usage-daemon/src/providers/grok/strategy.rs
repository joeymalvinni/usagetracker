//! Grok source selection policy.

use crate::providers::{ProviderError, ProviderErrorKind};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SourceMode {
    #[default]
    Auto,
    Cli,
    Web,
}

impl SourceMode {
    pub(super) fn parse(value: Option<&str>) -> Result<Self, ProviderError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("auto") => Ok(Self::Auto),
            Some("cli") => Ok(Self::Cli),
            Some("web") => Ok(Self::Web),
            Some(value) => Err(ProviderError::new(
                ProviderErrorKind::Parse,
                format!("unsupported Grok source_mode `{value}`; expected auto, cli, or web"),
            )),
        }
    }

    pub(super) fn uses_cli(self) -> bool {
        matches!(self, Self::Auto | Self::Cli)
    }

    pub(super) fn uses_web(self) -> bool {
        matches!(self, Self::Auto | Self::Web)
    }

    pub(super) fn permits_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_modes_and_rejects_ambiguous_values() {
        assert_eq!(SourceMode::parse(None).unwrap(), SourceMode::Auto);
        assert_eq!(SourceMode::parse(Some("cli")).unwrap(), SourceMode::Cli);
        assert_eq!(SourceMode::parse(Some("web")).unwrap(), SourceMode::Web);
        assert!(SourceMode::parse(Some("oauth")).is_err());
    }
}
