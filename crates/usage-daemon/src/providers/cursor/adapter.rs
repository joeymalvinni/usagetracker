use std::{sync::Arc, time::Duration};

use crate::{
    config::ProviderConfig,
    providers::ProviderCollector,
    runtime::provider_adapter::{ExecutionPolicy, ProviderAdapter, ProviderManifest},
};

use super::{settings, CursorCollector, CURSOR_PROVIDER_ID};

pub(crate) static ADAPTER: CursorAdapter = CursorAdapter;

pub(crate) struct CursorAdapter;

impl ProviderAdapter for CursorAdapter {
    fn manifest(&self) -> ProviderManifest {
        ProviderManifest {
            id: CURSOR_PROVIDER_ID,
            display_name: "Cursor",
            minimum_refresh_interval_seconds: 60,
            default_visible: false,
        }
    }

    fn execution_policy(&self) -> ExecutionPolicy {
        ExecutionPolicy::new(Duration::from_secs(30), Duration::from_secs(60), 4)
    }

    fn supports_multiple_accounts(&self) -> bool {
        true
    }

    fn validate_config(&self, config: &ProviderConfig) -> anyhow::Result<()> {
        settings::validate(config)
    }

    fn build_collector(
        &self,
        config: &ProviderConfig,
    ) -> anyhow::Result<Arc<dyn ProviderCollector>> {
        Ok(Arc::new(CursorCollector::new(config.clone())?))
    }
}
