use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Context};
use usage_core::{
    aggregate_usage_dashboard, Account, ConfigResponse, Connectivity, ProviderCapabilities,
    ProviderDescriptor, ProviderHealth, StateSnapshot, UsageDashboardSummary, UsageForecast,
    UsageSnapshot, UsageWindowProvenance,
};

const ALIASES: &[(&str, &str)] = &[
    ("codex", "codex"),
    ("claude", "claude"),
    ("grok", "grok"),
    ("xai", "grok"),
    ("opencode", "opencode_go"),
    ("opencode-go", "opencode_go"),
    ("opencode_go", "opencode_go"),
];

const RESERVED: &[&str] = &[
    "summary",
    "activity",
    "status",
    "health",
    "refresh",
    "accounts",
    "providers",
    "config",
    "usage",
    "help",
    "version",
];

#[derive(Clone, Debug)]
pub struct ProviderInfo {
    pub display_name: String,
    pub enabled: bool,
    pub capabilities: ProviderCapabilities,
}

#[derive(Clone, Debug)]
pub struct ProviderCatalog {
    providers: BTreeMap<String, ProviderInfo>,
    order: Vec<String>,
}

impl ProviderCatalog {
    pub fn from_state(state: &StateSnapshot) -> Self {
        let mut providers = BTreeMap::new();
        let mut order = Vec::new();
        for descriptor in &state.server.providers {
            insert_descriptor(&mut providers, &mut order, descriptor, &state.config);
        }
        for provider_id in state.config.providers.keys() {
            if !providers.contains_key(provider_id) {
                order.push(provider_id.clone());
                providers.insert(
                    provider_id.clone(),
                    ProviderInfo {
                        display_name: display_name_fallback(provider_id),
                        enabled: state
                            .config
                            .providers
                            .get(provider_id)
                            .is_some_and(|toggle| toggle.enabled),
                        capabilities: ProviderCapabilities::default(),
                    },
                );
            }
        }
        for provider_id in state
            .snapshots
            .iter()
            .map(|row| row.provider_id.as_str())
            .chain(
                state
                    .dashboard
                    .accounts
                    .iter()
                    .map(|row| row.provider_id.as_str()),
            )
        {
            if !providers.contains_key(provider_id) {
                order.push(provider_id.to_string());
                providers.insert(
                    provider_id.to_string(),
                    ProviderInfo {
                        display_name: display_name_fallback(provider_id),
                        enabled: false,
                        capabilities: ProviderCapabilities::default(),
                    },
                );
            }
        }
        Self { providers, order }
    }

    pub fn resolve(&self, token: &str) -> Result<String, ResolveError> {
        if self.providers.contains_key(token) {
            return Ok(token.to_string());
        }
        if let Some((_, canonical)) = ALIASES
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(token))
        {
            if self.providers.contains_key(*canonical) {
                return Ok((*canonical).to_string());
            }
        }
        Err(ResolveError {
            token: token.to_string(),
            suggestion: self.suggestion(token),
        })
    }

    pub fn resolve_many(&self, values: &[String]) -> Result<Vec<String>, ResolveError> {
        let mut seen = HashSet::new();
        values
            .iter()
            .map(|value| self.resolve(value))
            .filter_map(|result| match result {
                Ok(id) if seen.insert(id.clone()) => Some(Ok(id)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub fn info(&self, provider_id: &str) -> Option<&ProviderInfo> {
        self.providers.get(provider_id)
    }

    pub fn ordered_ids(&self, all_providers: bool) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| {
                all_providers
                    || self
                        .providers
                        .get(*id)
                        .is_some_and(|provider| provider.enabled)
            })
            .cloned()
            .collect()
    }

    fn suggestion(&self, token: &str) -> Option<String> {
        let normalized = token.to_ascii_lowercase();
        let prefix = normalized.chars().take(3).collect::<String>();
        let aliases = ALIASES.iter().map(|(alias, _)| *alias);
        let candidates = self
            .providers
            .keys()
            .map(String::as_str)
            .chain(aliases)
            .chain(RESERVED.iter().copied());
        candidates
            .map(|candidate| (edit_distance(&normalized, candidate), candidate))
            .filter(|(distance, candidate)| {
                *distance <= 2
                    || (token.len() >= 4 && candidate.to_ascii_lowercase().starts_with(&prefix))
            })
            .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
            .map(|(_, candidate)| candidate.to_string())
    }
}

fn insert_descriptor(
    providers: &mut BTreeMap<String, ProviderInfo>,
    order: &mut Vec<String>,
    descriptor: &ProviderDescriptor,
    config: &ConfigResponse,
) {
    let id = descriptor.id.as_str().to_string();
    order.push(id.clone());
    providers.insert(
        id.clone(),
        ProviderInfo {
            display_name: descriptor.display_name.clone(),
            enabled: config
                .providers
                .get(descriptor.id.as_str())
                .is_some_and(|toggle| toggle.enabled),
            capabilities: descriptor.capabilities,
        },
    );
}

#[derive(Clone, Debug)]
pub struct ResolveError {
    pub token: String,
    pub suggestion: Option<String>,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "unknown provider '{}'", self.token)?;
        if let Some(suggestion) = &self.suggestion {
            write!(formatter, "; did you mean '{suggestion}'?")?;
        }
        Ok(())
    }
}

impl std::error::Error for ResolveError {}

#[derive(Clone, Debug, Default)]
pub struct SelectionRequest {
    pub providers: Vec<String>,
    pub accounts: Vec<String>,
    pub all_providers: bool,
}

#[derive(Debug)]
pub struct SelectedState {
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub connectivity: Connectivity,
    pub catalog: ProviderCatalog,
    pub provider_ids: Vec<String>,
    pub config: ConfigResponse,
    pub accounts: Vec<Account>,
    pub health: Vec<ProviderHealth>,
    pub snapshots: Vec<UsageSnapshot>,
    pub dashboard: UsageDashboardSummary,
    pub forecasts: Vec<UsageForecast>,
    pub window_provenance: Vec<UsageWindowProvenance>,
}

impl SelectedState {
    pub fn from_state(mut state: StateSnapshot, request: SelectionRequest) -> anyhow::Result<Self> {
        let catalog = ProviderCatalog::from_state(&state);
        let provider_ids = if request.providers.is_empty() {
            catalog.ordered_ids(request.all_providers)
        } else {
            catalog
                .resolve_many(&request.providers)
                .map_err(anyhow::Error::new)?
        };
        let selected_providers = provider_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();

        validate_accounts(&state.accounts, &request.accounts, &selected_providers)?;
        let selected_accounts = request
            .accounts
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let hidden_accounts = state
            .accounts
            .iter()
            .filter(|account| account.hidden)
            .map(|account| account.id.to_string())
            .collect::<HashSet<_>>();
        let include_account = |provider: &str, account: &str| {
            selected_providers.contains(provider)
                && !hidden_accounts.contains(account)
                && (selected_accounts.is_empty() || selected_accounts.contains(account))
        };

        state
            .accounts
            .retain(|account| include_account(account.provider_id.as_str(), account.id.as_str()));
        state.snapshots.retain(|snapshot| {
            include_account(snapshot.provider_id.as_str(), snapshot.account_id.as_str())
        });
        state.forecasts.retain(|forecast| {
            include_account(forecast.provider_id.as_str(), forecast.account_id.as_str())
        });
        state
            .window_provenance
            .retain(|row| include_account(row.provider_id.as_str(), row.account_id.as_str()));
        state.health.retain(|row| {
            selected_providers.contains(row.provider_id.as_str())
                && row
                    .account_id
                    .as_ref()
                    .is_none_or(|id| include_account(row.provider_id.as_str(), id.as_str()))
        });
        state.dashboard.accounts.retain(|summary| {
            include_account(summary.provider_id.as_str(), summary.account_id.as_str())
        });
        let dashboard = aggregate_usage_dashboard(state.dashboard.accounts);

        Ok(Self {
            generated_at: state.generated_at,
            connectivity: state.connectivity,
            catalog,
            provider_ids,
            config: state.config,
            accounts: state.accounts,
            health: state.health,
            snapshots: state.snapshots,
            dashboard,
            forecasts: state.forecasts,
            window_provenance: state.window_provenance,
        })
    }

    pub fn provider_info(&self, provider_id: &str) -> &ProviderInfo {
        self.catalog.info(provider_id).unwrap_or_else(|| {
            panic!("selected provider {provider_id} must exist in provider catalog")
        })
    }
}

fn validate_accounts(
    accounts: &[Account],
    requested: &[String],
    selected_providers: &HashSet<&str>,
) -> anyhow::Result<()> {
    for requested_id in requested {
        let account = accounts
            .iter()
            .find(|account| account.id.as_str() == requested_id)
            .with_context(|| format!("unknown account '{requested_id}'"))?;
        if account.hidden {
            bail!(
                "account '{}' is hidden; run `usage accounts show {}` first",
                requested_id,
                requested_id
            );
        }
        if !selected_providers.contains(account.provider_id.as_str()) {
            bail!(
                "account '{}' belongs to provider '{}', which is not selected",
                requested_id,
                account.provider_id
            );
        }
    }
    Ok(())
}

pub fn latest_snapshots(snapshots: &[UsageSnapshot]) -> BTreeMap<(&str, &str), &UsageSnapshot> {
    let mut latest = BTreeMap::new();
    for snapshot in snapshots {
        let key = (snapshot.provider_id.as_str(), snapshot.account_id.as_str());
        if latest
            .get(&key)
            .is_none_or(|current: &&UsageSnapshot| snapshot.collected_at >= current.collected_at)
        {
            latest.insert(key, snapshot);
        }
    }
    latest
}

fn display_name_fallback(provider_id: &str) -> String {
    provider_id
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                format!("{}{}", first.to_uppercase(), chars.as_str())
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right.chars().enumerate() {
            current.push(
                (previous[right_index + 1] + 1)
                    .min(current[right_index] + 1)
                    .min(previous[right_index] + usize::from(left_char != right_char)),
            );
        }
        previous = current;
    }
    previous.last().copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_core::{
        ApiResponse, ProviderDescriptor, ProviderId, ProviderToggle, ResponseEnvelope,
    };

    fn fixture_state() -> StateSnapshot {
        let envelope: ResponseEnvelope =
            serde_json::from_str(include_str!("../../usage-core/wire-fixtures/state_v3.json"))
                .unwrap();
        let ApiResponse::State { state } = envelope.response else {
            panic!("expected state fixture");
        };
        state
    }

    #[test]
    fn edit_distance_handles_insertions_and_substitutions() {
        assert_eq!(edit_distance("claud", "claude"), 1);
        assert_eq!(edit_distance("grok", "grok"), 0);
        assert_eq!(edit_distance("codex", "claude"), 4);
    }

    #[test]
    fn resolver_prefers_exact_daemon_id_over_builtin_alias() {
        let mut state = fixture_state();
        for id in ["grok", "xai"] {
            state
                .config
                .providers
                .insert(id.to_string(), ProviderToggle { enabled: true });
            state.server.providers.push(ProviderDescriptor {
                id: ProviderId::new(id),
                display_name: id.to_uppercase(),
                minimum_refresh_interval_seconds: 0,
                capabilities: ProviderCapabilities::default(),
            });
        }
        let catalog = ProviderCatalog::from_state(&state);

        assert_eq!(catalog.resolve("xai").unwrap(), "xai");
        assert_eq!(catalog.resolve("XAI").unwrap(), "grok");
    }

    #[test]
    fn resolver_normalizes_builtin_aliases_and_suggests_typos() {
        let mut state = fixture_state();
        state
            .config
            .providers
            .insert("opencode_go".to_string(), ProviderToggle { enabled: true });
        let catalog = ProviderCatalog::from_state(&state);

        assert_eq!(catalog.resolve("CODEX").unwrap(), "codex");
        assert_eq!(catalog.resolve("opencode-go").unwrap(), "opencode_go");
        assert_eq!(
            catalog.resolve("claud").unwrap_err().suggestion.as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn default_selection_keeps_enabled_no_data_provider() {
        let selected =
            SelectedState::from_state(fixture_state(), SelectionRequest::default()).unwrap();

        assert_eq!(selected.provider_ids, ["codex"]);
        assert!(selected.snapshots.is_empty());
        assert!(selected.dashboard.accounts.is_empty());
    }
}
