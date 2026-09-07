//! Typed replacement for the former untyped `diagnostics` JSON blob carried on
//! [`UsageSnapshot`](crate::UsageSnapshot).
//!
//! Provider collectors, the merge layer, storage, the dashboard builder and the
//! CLI all used to spelunk this blob with string keys, so a provider renaming a
//! field produced a silently empty dashboard rather than a compile error. Every
//! field a consumer actually reads is modeled here as a first-class type shared
//! by the write and read sides. Purely diagnostic, write-only keys that no
//! consumer parses are preserved verbatim in [`SnapshotDetail::extra`] so they
//! still round-trip and remain visible under `diagnostics` for debugging.

use chrono::{DateTime, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{DailyUsagePoint, DatasetProvenance, ModelCostSummary};

/// Canonical top-level keys used by the merge layer to track which dataset
/// contributed each typed field. Extra (diagnostic) keys use their own name.
pub const COST_KEY: &str = "cost";
pub const ACTIVITY_KEY: &str = "activity";
pub const RESET_CREDITS_KEY: &str = "reset_credits";

const SCALAR_KEYS: &[&str] = &[
    "web_authoritative",
    "estimate",
    "email",
    "account_email",
    "account_display_name",
    "credential_profile",
    "keychain_account",
    "plan_type",
    "subscription_type",
    "collection_mode",
];

/// Typed contents of a snapshot's `diagnostics` blob.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SnapshotDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCreditsDetail>,
    /// Typed origin of each contributed dataset. Written by the merge layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dataset_provenance: Vec<DatasetProvenance>,

    /// Set to `false` by collectors that produce a synthetic local estimate
    /// rather than a web-authoritative reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_authoritative: Option<bool>,
    /// Top-level estimate flag consulted when no dataset provenance matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<bool>,

    // Identity / plan labels read by the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keychain_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_mode: Option<String>,

    /// Purely diagnostic, write-only keys that no consumer parses (e.g.
    /// `top_level_keys`, `files_scanned`, `zen_balance_usd`). Preserved so they
    /// round-trip and stay visible for debugging.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Normalized cost/spend summary for one account. Shared by every provider's
/// cost collector and by the dashboard builder.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CostDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub estimate: bool,
    #[serde(default)]
    pub partial: bool,
    /// Some providers report whether the lookback window was fully scanned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete_lookback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub today_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub today_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Unpriced token count for the synthesized "today" point when no daily
    /// breakdown is present.
    #[serde(default)]
    pub unpriced_tokens: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<UnpricedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_effective_from: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_day: Vec<DailyUsagePoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<ModelCostSummary>,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Normalized token-activity summary for one account.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActivityDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub today_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_day: Vec<DailyUsagePoint>,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Rate-limit reset credits (Codex). Timestamps are normalized to UTC by the
/// collector so consumers never re-parse epoch seconds or ISO strings.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ResetCreditsDetail {
    #[serde(default)]
    pub available_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credits: Vec<ResetCreditEntry>,
}

/// A single reset credit entry.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ResetCreditEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// A model whose tokens could not be priced. Deserializes from either a bare
/// model-name string or a `{ "model", "tokens" }` object.
#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize)]
pub struct UnpricedModel {
    pub model: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tokens: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl<'de> Deserialize<'de> for UnpricedModel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Name(String),
            Object {
                model: String,
                #[serde(default)]
                tokens: u64,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Name(model) => UnpricedModel { model, tokens: 0 },
            Raw::Object { model, tokens } => UnpricedModel { model, tokens },
        })
    }
}

impl SnapshotDetail {
    /// Keys this detail currently carries, in the vocabulary the merge layer
    /// records per dataset so a later dataset can replace exactly its own
    /// contributions. Typed fields use the [`COST_KEY`]/[`ACTIVITY_KEY`]/
    /// [`RESET_CREDITS_KEY`] and scalar-field names; diagnostic keys use their
    /// own name.
    pub fn present_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if self.cost.is_some() {
            keys.push(COST_KEY.to_string());
        }
        if self.activity.is_some() {
            keys.push(ACTIVITY_KEY.to_string());
        }
        if self.reset_credits.is_some() {
            keys.push(RESET_CREDITS_KEY.to_string());
        }
        for key in SCALAR_KEYS {
            if self.has_scalar(key) {
                keys.push((*key).to_string());
            }
        }
        keys.extend(self.extra.keys().cloned());
        keys
    }

    /// Copies every key present in `other` but absent here, returning the keys
    /// actually contributed. First-writer-wins, matching the former key-union
    /// JSON merge.
    pub fn fill_missing_from(&mut self, other: &SnapshotDetail) -> Vec<String> {
        let mut contributed = Vec::new();
        for key in other.present_keys() {
            if !self.contains_key(&key) {
                self.copy_key_from(&key, other);
                contributed.push(key);
            }
        }
        contributed
    }

    /// Removes the listed keys (typed fields cleared to `None`, diagnostic keys
    /// dropped from `extra`).
    pub fn remove_keys<I, S>(&mut self, keys: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for key in keys {
            self.clear_key(key.as_ref());
        }
    }

    fn contains_key(&self, key: &str) -> bool {
        match key {
            COST_KEY => self.cost.is_some(),
            ACTIVITY_KEY => self.activity.is_some(),
            RESET_CREDITS_KEY => self.reset_credits.is_some(),
            _ if SCALAR_KEYS.contains(&key) => self.has_scalar(key),
            _ => self.extra.contains_key(key),
        }
    }

    fn copy_key_from(&mut self, key: &str, other: &SnapshotDetail) {
        match key {
            COST_KEY => self.cost = other.cost.clone(),
            ACTIVITY_KEY => self.activity = other.activity.clone(),
            RESET_CREDITS_KEY => self.reset_credits = other.reset_credits.clone(),
            "web_authoritative" => self.web_authoritative = other.web_authoritative,
            "estimate" => self.estimate = other.estimate,
            "email" => self.email = other.email.clone(),
            "account_email" => self.account_email = other.account_email.clone(),
            "account_display_name" => {
                self.account_display_name = other.account_display_name.clone()
            }
            "credential_profile" => self.credential_profile = other.credential_profile.clone(),
            "keychain_account" => self.keychain_account = other.keychain_account.clone(),
            "plan_type" => self.plan_type = other.plan_type.clone(),
            "subscription_type" => self.subscription_type = other.subscription_type.clone(),
            "collection_mode" => self.collection_mode = other.collection_mode.clone(),
            _ => {
                if let Some(value) = other.extra.get(key) {
                    self.extra.insert(key.to_string(), value.clone());
                }
            }
        }
    }

    fn clear_key(&mut self, key: &str) {
        match key {
            COST_KEY => self.cost = None,
            ACTIVITY_KEY => self.activity = None,
            RESET_CREDITS_KEY => self.reset_credits = None,
            "web_authoritative" => self.web_authoritative = None,
            "estimate" => self.estimate = None,
            "email" => self.email = None,
            "account_email" => self.account_email = None,
            "account_display_name" => self.account_display_name = None,
            "credential_profile" => self.credential_profile = None,
            "keychain_account" => self.keychain_account = None,
            "plan_type" => self.plan_type = None,
            "subscription_type" => self.subscription_type = None,
            "collection_mode" => self.collection_mode = None,
            _ => {
                self.extra.remove(key);
            }
        }
    }

    fn has_scalar(&self, key: &str) -> bool {
        match key {
            "web_authoritative" => self.web_authoritative.is_some(),
            "estimate" => self.estimate.is_some(),
            "email" => self.email.is_some(),
            "account_email" => self.account_email.is_some(),
            "account_display_name" => self.account_display_name.is_some(),
            "credential_profile" => self.credential_profile.is_some(),
            "keychain_account" => self.keychain_account.is_some(),
            "plan_type" => self.plan_type.is_some(),
            "subscription_type" => self.subscription_type.is_some(),
            "collection_mode" => self.collection_mode.is_some(),
            _ => false,
        }
    }
}
