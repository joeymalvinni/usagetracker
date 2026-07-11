use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use usage_core::{AccountId, PendingNotification};

use super::{parse_time_sql, NotificationWindowState, Storage};

impl Storage {
    pub async fn notification_window_state(
        &self,
        account_id: &AccountId,
        window_id: &str,
    ) -> anyhow::Result<Option<NotificationWindowState>> {
        let account_id = account_id.clone();
        let window_id = window_id.to_string();
        self.with_connection(move |conn| {
            let row = conn
                .query_row(
                    "SELECT reset_at, notified_mask, last_attempt_at
                     FROM notification_window_state
                     WHERE account_id = ?1 AND window_id = ?2",
                    params![account_id.as_str(), window_id],
                    |row| {
                        let reset_at: Option<String> = row.get(0)?;
                        let notified_mask: i64 = row.get(1)?;
                        let last_attempt_at: Option<String> = row.get(2)?;
                        Ok((reset_at, notified_mask, last_attempt_at))
                    },
                )
                .optional()?;
            row.map(|(reset_at, notified_mask, last_attempt_at)| {
                Ok(NotificationWindowState {
                    reset_at: reset_at.as_deref().map(parse_time_sql).transpose()?,
                    notified_mask: u8::try_from(notified_mask)?,
                    last_attempt_at: last_attempt_at.as_deref().map(parse_time_sql).transpose()?,
                })
            })
            .transpose()
        })
        .await
    }

    pub async fn upsert_notification_window_state(
        &self,
        account_id: &AccountId,
        window_id: &str,
        state: NotificationWindowState,
    ) -> anyhow::Result<()> {
        let account_id = account_id.clone();
        let window_id = window_id.to_string();
        self.with_connection(move |conn| {
            conn.execute(
                "INSERT INTO notification_window_state
                 (account_id, window_id, reset_at, notified_mask, last_attempt_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, window_id) DO UPDATE SET
                   reset_at = excluded.reset_at,
                   notified_mask = excluded.notified_mask,
                   last_attempt_at = excluded.last_attempt_at",
                params![
                    account_id.as_str(),
                    window_id,
                    state.reset_at.map(|value| value.to_rfc3339()),
                    i64::from(state.notified_mask),
                    state.last_attempt_at.map(|value| value.to_rfc3339()),
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn clear_notification_window_state(&self) -> anyhow::Result<()> {
        self.with_connection(|conn| {
            conn.execute("DELETE FROM notification_window_state", [])?;
            Ok(())
        })
        .await
    }

    pub async fn enqueue_notification(&self, title: &str, body: &str) -> anyhow::Result<()> {
        let title = title.to_string();
        let body = body.to_string();
        self.with_connection(move |conn| {
            conn.execute(
                "INSERT INTO pending_notifications (title, body, created_at) VALUES (?1, ?2, ?3)",
                params![title, body, Utc::now().to_rfc3339()],
            )?;
            conn.execute(
                "DELETE FROM pending_notifications WHERE id NOT IN
                 (SELECT id FROM pending_notifications ORDER BY id DESC LIMIT 1000)",
                [],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn pending_notifications(&self) -> anyhow::Result<Vec<PendingNotification>> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, body, created_at FROM pending_notifications
                 ORDER BY id ASC LIMIT 100",
            )?;
            let notifications = stmt
                .query_map([], |row| {
                    let created_at: String = row.get(3)?;
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        created_at,
                    ))
                })?
                .map(|row| {
                    let (id, title, body, created_at) = row?;
                    Ok(PendingNotification {
                        id,
                        title,
                        body,
                        created_at: parse_time_sql(&created_at)?,
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(notifications)
        })
        .await
    }

    pub async fn acknowledge_notifications(&self, ids: &[i64]) -> anyhow::Result<()> {
        let ids = ids.to_vec();
        self.with_connection(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            for id in ids {
                transaction.execute("DELETE FROM pending_notifications WHERE id = ?1", [id])?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn clear_pending_notifications(&self) -> anyhow::Result<()> {
        self.with_connection(|conn| {
            conn.execute("DELETE FROM pending_notifications", [])?;
            Ok(())
        })
        .await
    }
}
