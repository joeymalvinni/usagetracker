use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Row};
use usage_core::{Account, AccountDisplayNameSource, AccountId, ProviderId};
use uuid::Uuid;

use super::{parse_time_sql, AccountIdentityConflict, Storage};

impl Storage {
    pub async fn upsert_account(
        &self,
        provider_id: &ProviderId,
        external_account_id: &str,
        profile_id: Option<&str>,
        display_name: Option<&str>,
        email: Option<&str>,
    ) -> anyhow::Result<Account> {
        let provider_id = provider_id.clone();
        let external_account_id = external_account_id.to_string();
        let profile_id = normalized_profile_id(profile_id, &external_account_id);
        let display_name = normalized_identity_value(display_name);
        let email = normalized_email(email).or_else(|| {
            display_name
                .as_deref()
                .filter(|value| looks_like_email(value))
                .map(ToOwned::to_owned)
        });
        let display_name = display_name.filter(|value| !looks_like_email(value));
        self.with_connection(move |conn| {
            let now = Utc::now();
            let existing = conn
                .query_row(
                    account_select_sql("WHERE provider_id = ?1 AND profile_id = ?2").as_str(),
                    params![provider_id.as_str(), profile_id.as_str()],
                    account_from_row,
                )
                .optional()?;

            let adopting_legacy_identity = existing.as_ref().is_some_and(|existing| {
                can_adopt_legacy_external_identity(
                    &provider_id,
                    &existing.external_account_id,
                    &external_account_id,
                )
            });
            if let Some(existing) = existing.as_ref() {
                if existing.external_account_id != external_account_id && !adopting_legacy_identity
                {
                    return Err(AccountIdentityConflict::ProfileChanged {
                        provider_id: provider_id.as_str().to_string(),
                        profile_id: profile_id.clone(),
                        stored_external_account_id: existing.external_account_id.clone(),
                        discovered_external_account_id: external_account_id.clone(),
                    }
                    .into());
                }
            }
            if provider_requires_unique_external_account(&provider_id)
                && (existing.is_none() || adopting_legacy_identity)
            {
                let existing_profile_id = conn
                    .query_row(
                        "SELECT profile_id FROM accounts
                         WHERE provider_id = ?1 AND external_account_id = ?2 AND profile_id != ?3
                         LIMIT 1",
                        params![
                            provider_id.as_str(),
                            external_account_id.as_str(),
                            profile_id.as_str()
                        ],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(existing_profile_id) = existing_profile_id {
                    return Err(AccountIdentityConflict::DuplicateExternalAccount {
                        provider_id: provider_id.as_str().to_string(),
                        external_account_id: external_account_id.clone(),
                        existing_profile_id,
                        discovered_profile_id: profile_id.clone(),
                    }
                    .into());
                }
            }

            let (
                id,
                created_at,
                hidden,
                collection_enabled,
                next_display_name,
                display_name_source,
                next_email,
            ) = if let Some(existing) = existing {
                let (next_display_name, display_name_source) =
                    if existing.display_name_source == AccountDisplayNameSource::User {
                        (existing.display_name, AccountDisplayNameSource::User)
                    } else if let Some(display_name) = display_name {
                        (Some(display_name), AccountDisplayNameSource::User)
                    } else {
                        (existing.display_name, existing.display_name_source)
                    };
                (
                    existing.id.to_string(),
                    existing.created_at,
                    existing.hidden,
                    existing.collection_enabled,
                    next_display_name,
                    display_name_source,
                    email.or(existing.email),
                )
            } else {
                let (next_display_name, display_name_source) = match display_name {
                    Some(display_name) => (Some(display_name), AccountDisplayNameSource::User),
                    None => (
                        Some(generated_account_display_name(conn, provider_id.as_str())?),
                        AccountDisplayNameSource::Generated,
                    ),
                };
                (
                    Uuid::new_v4().to_string(),
                    now,
                    false,
                    true,
                    next_display_name,
                    display_name_source,
                    email,
                )
            };
            conn.execute(
                "INSERT INTO accounts
             (id, provider_id, external_account_id, profile_id, display_name, display_name_source,
              email, hidden, collection_enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(provider_id, profile_id) DO UPDATE SET
               external_account_id = excluded.external_account_id,
               display_name = excluded.display_name,
               display_name_source = excluded.display_name_source,
               email = excluded.email,
               updated_at = excluded.updated_at",
                params![
                    id,
                    provider_id.as_str(),
                    external_account_id.as_str(),
                    profile_id.as_str(),
                    next_display_name.as_deref(),
                    display_name_source_sql(display_name_source),
                    next_email.as_deref(),
                    i64::from(hidden),
                    i64::from(collection_enabled),
                    created_at.to_rfc3339(),
                    now.to_rfc3339(),
                ],
            )?;

            Ok(Account {
                id: AccountId::new(id),
                provider_id,
                external_account_id,
                profile_id: (!profile_id.is_empty()).then_some(profile_id),
                display_name: next_display_name,
                display_name_source,
                email: next_email,
                hidden,
                collection_enabled,
                created_at,
                updated_at: now,
            })
        })
        .await
    }
    pub async fn account(&self, account_id: &AccountId) -> anyhow::Result<Option<Account>> {
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            conn.query_row(
                account_select_sql("WHERE id = ?1").as_str(),
                params![account_id.as_str()],
                account_from_row,
            )
            .optional()
            .map_err(Into::into)
        })
        .await
    }

    pub async fn update_account(
        &self,
        account_id: &AccountId,
        display_name: Option<&str>,
        hidden: Option<bool>,
        collection_enabled: Option<bool>,
    ) -> anyhow::Result<Account> {
        let account_id = account_id.clone();
        let display_name = display_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        self.with_connection(move |conn| {
            let existing = conn
                .query_row(
                    account_select_sql("WHERE id = ?1").as_str(),
                    params![account_id.as_str()],
                    account_from_row,
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("unknown account: {}", account_id.as_str()))?;
            let next_display_name = display_name.as_deref().or(existing.display_name.as_deref());
            let next_display_name_source = if display_name.is_some() {
                AccountDisplayNameSource::User
            } else {
                existing.display_name_source
            };
            let next_hidden = hidden.unwrap_or(existing.hidden);
            let next_collection_enabled = collection_enabled.unwrap_or(existing.collection_enabled);
            let updated_at = Utc::now();
            conn.execute(
                "UPDATE accounts
                 SET display_name = ?1,
                     display_name_source = ?2,
                     hidden = ?3,
                     collection_enabled = ?4,
                     updated_at = ?5
                 WHERE id = ?6",
                params![
                    next_display_name,
                    display_name_source_sql(next_display_name_source),
                    i64::from(next_hidden),
                    i64::from(next_collection_enabled),
                    updated_at.to_rfc3339(),
                    account_id.as_str(),
                ],
            )?;
            Ok(Account {
                display_name: next_display_name.map(ToOwned::to_owned),
                display_name_source: next_display_name_source,
                hidden: next_hidden,
                collection_enabled: next_collection_enabled,
                updated_at,
                ..existing
            })
        })
        .await
    }

    pub async fn delete_account(&self, account_id: &AccountId) -> anyhow::Result<()> {
        let account_id = account_id.clone();
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            transaction.execute(
                "DELETE FROM usage_snapshots WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM provider_health WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM provider_daily_usage WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
            let deleted = transaction.execute(
                "DELETE FROM accounts WHERE id = ?1",
                params![account_id.as_str()],
            )?;
            if deleted == 0 {
                anyhow::bail!("unknown account: {}", account_id.as_str());
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn accounts(&self) -> anyhow::Result<Vec<Account>> {
        self.with_connection(accounts_from_conn).await
    }
}

pub(super) fn accounts_from_conn(conn: &Connection) -> anyhow::Result<Vec<Account>> {
    let mut stmt = conn.prepare(
        account_select_sql("ORDER BY provider_id, profile_id, external_account_id").as_str(),
    )?;
    let accounts = stmt
        .query_map([], account_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(accounts)
}

fn account_from_row(row: &Row<'_>) -> rusqlite::Result<Account> {
    let profile_id: String = row.get(3)?;
    let display_name_source: String = row.get(5)?;
    let created_at: String = row.get(9)?;
    let updated_at: String = row.get(10)?;
    Ok(Account {
        id: AccountId::new(row.get::<_, String>(0)?),
        provider_id: ProviderId::new(row.get::<_, String>(1)?),
        external_account_id: row.get(2)?,
        profile_id: (!profile_id.is_empty()).then_some(profile_id),
        display_name: row.get(4)?,
        display_name_source: display_name_source_from_sql(&display_name_source),
        email: row.get(6)?,
        hidden: row.get::<_, i64>(7)? != 0,
        collection_enabled: row.get::<_, i64>(8)? != 0,
        created_at: parse_time_sql(&created_at)?,
        updated_at: parse_time_sql(&updated_at)?,
    })
}

fn account_select_sql(suffix: &str) -> String {
    format!(
        "SELECT id, provider_id, external_account_id, profile_id, display_name,
                display_name_source, email, hidden, collection_enabled, created_at, updated_at
         FROM accounts
         {suffix}"
    )
}

fn normalized_profile_id(profile_id: Option<&str>, external_account_id: &str) -> String {
    profile_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(external_account_id)
        .to_string()
}

fn provider_requires_unique_external_account(provider_id: &ProviderId) -> bool {
    matches!(provider_id.as_str(), "codex" | "claude" | "grok")
}

fn can_adopt_legacy_external_identity(
    provider_id: &ProviderId,
    stored_external_account_id: &str,
    discovered_external_account_id: &str,
) -> bool {
    (provider_id.as_str() == "claude"
        && !is_canonical_uuid(stored_external_account_id)
        && is_canonical_uuid(discovered_external_account_id))
        || (provider_id.as_str() == "grok"
            && stored_external_account_id == "grok_default"
            && discovered_external_account_id != "grok_default")
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .is_ok_and(|uuid| uuid.hyphenated().to_string().eq_ignore_ascii_case(value))
}

fn normalized_identity_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalized_email(value: Option<&str>) -> Option<String> {
    normalized_identity_value(value).filter(|value| looks_like_email(value))
}

pub(super) fn looks_like_email(value: &str) -> bool {
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.chars().any(char::is_whitespace)
}

fn display_name_source_sql(source: AccountDisplayNameSource) -> &'static str {
    match source {
        AccountDisplayNameSource::Provider => "provider",
        AccountDisplayNameSource::Generated => "generated",
        AccountDisplayNameSource::User => "user",
    }
}

fn display_name_source_from_sql(value: &str) -> AccountDisplayNameSource {
    match value {
        "user" => AccountDisplayNameSource::User,
        "provider" => AccountDisplayNameSource::Provider,
        _ => AccountDisplayNameSource::Generated,
    }
}

fn generated_account_display_name(conn: &Connection, provider_id: &str) -> anyhow::Result<String> {
    let mut stmt = conn.prepare("SELECT display_name FROM accounts WHERE provider_id = ?1")?;
    let existing = stmt
        .query_map(params![provider_id], |row| row.get::<_, Option<String>>(0))?
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>, _>>()?;

    let (base, always_numbered) = match provider_id {
        "codex" => ("Codex".to_string(), true),
        "claude" => ("Claude".to_string(), true),
        "opencode_go" => ("OpenCode Go".to_string(), false),
        value => (
            value
                .split(['_', '-'])
                .filter(|part| !part.is_empty())
                .map(|part| {
                    let mut chars = part.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" "),
            true,
        ),
    };

    if !always_numbered && !existing.iter().any(|name| name.eq_ignore_ascii_case(&base)) {
        return Ok(base);
    }
    for ordinal in 1_u64.. {
        let candidate = format!("{base} {ordinal}");
        if !existing
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&candidate))
        {
            return Ok(candidate);
        }
    }
    unreachable!("account label ordinal space is exhausted")
}
