use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::Arc,
};

use chrono::Utc;
use tokio::sync::Mutex;
use usage_core::{AccountId, ImportJob, ImportJobId, ImportJobStatus};

const RETAINED_IMPORT_JOBS: usize = 64;

#[derive(Default)]
struct Inner {
    by_id: HashMap<ImportJobId, ImportJob>,
    active_by_account: HashMap<AccountId, ImportJobId>,
    completed: VecDeque<ImportJobId>,
}

/// In-memory registry of import jobs, sibling to the refresh job registry in
/// `polling.rs`. Enforces at most one active import per account and retains a
/// bounded history of finished jobs so `get` stays cheap.
#[derive(Default)]
pub struct ImportJobs {
    inner: Arc<Mutex<Inner>>,
}

impl ImportJobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `job` as queued and spawns `work` to run it to completion.
    /// Rejects the start if the job's account already has an active
    /// (non-terminal) import.
    pub async fn start<F, Fut>(&self, job: ImportJob, work: F) -> anyhow::Result<ImportJob>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let account_id = job.account_id.clone();
        let job_id = job.id.clone();
        {
            let mut inner = self.inner.lock().await;
            if inner.active_by_account.contains_key(&account_id) {
                anyhow::bail!(
                    "an import is already running for account {}",
                    account_id.as_str()
                );
            }
            inner
                .active_by_account
                .insert(account_id.clone(), job_id.clone());
            inner.by_id.insert(job_id.clone(), job.clone());
        }

        let inner = self.inner.clone();
        tokio::spawn(run_job(inner, account_id, job_id, work));

        Ok(job)
    }

    pub async fn get(&self, id: &ImportJobId) -> Option<ImportJob> {
        self.inner.lock().await.by_id.get(id).cloned()
    }

    pub async fn account_has_active_import(&self, account_id: &AccountId) -> bool {
        self.inner
            .lock()
            .await
            .active_by_account
            .contains_key(account_id)
    }
}

async fn run_job<F, Fut>(
    inner: Arc<Mutex<Inner>>,
    account_id: AccountId,
    job_id: ImportJobId,
    work: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    {
        let mut inner = inner.lock().await;
        if let Some(job) = inner.by_id.get_mut(&job_id) {
            job.status = ImportJobStatus::Running;
            job.started_at = Some(Utc::now());
        }
    }

    let result = work().await;

    {
        let mut guard = inner.lock().await;
        if let Some(job) = guard.by_id.get_mut(&job_id) {
            job.finished_at = Some(Utc::now());
            match result {
                Ok(()) => job.status = ImportJobStatus::Completed,
                Err(err) => {
                    job.status = ImportJobStatus::Failed;
                    job.failure_message = Some(err.to_string());
                }
            }
        }
        guard.active_by_account.remove(&account_id);
        guard.completed.push_back(job_id);
        while guard.completed.len() > RETAINED_IMPORT_JOBS {
            if let Some(expired) = guard.completed.pop_front() {
                guard.by_id.remove(&expired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use usage_core::{ImportMode, ImportOptions, ProviderId};

    use super::*;

    fn sample_job(status: ImportJobStatus) -> ImportJob {
        sample_job_for_account(status, "account-1")
    }

    fn sample_job_for_account(status: ImportJobStatus, account_id: &str) -> ImportJob {
        ImportJob {
            id: ImportJobId::new(uuid::Uuid::new_v4().to_string()),
            account_id: AccountId::new(account_id),
            provider_id: ProviderId::new("claude"),
            status,
            mode: ImportMode::PrefsOnly,
            options: ImportOptions::comfort_defaults(),
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            progress_message: None,
            failure_message: None,
        }
    }

    #[tokio::test]
    async fn start_returns_queued_job_and_get_reads_it() {
        let jobs = ImportJobs::new();
        let job = jobs
            .start(sample_job(ImportJobStatus::Queued), || async {
                Ok::<(), anyhow::Error>(())
            })
            .await
            .unwrap();
        assert_eq!(job.status, ImportJobStatus::Queued);
        let fetched = jobs.get(&job.id).await.unwrap();
        assert_eq!(fetched.id, job.id);
    }

    #[tokio::test]
    async fn rejects_second_active_import_for_same_account() {
        let jobs = ImportJobs::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate2 = gate.clone();
        let _first = jobs
            .start(sample_job(ImportJobStatus::Queued), move || {
                let gate2 = gate2.clone();
                async move {
                    gate2.notified().await;
                    Ok(())
                }
            })
            .await
            .unwrap();
        let err = jobs
            .start(sample_job(ImportJobStatus::Queued), || async { Ok(()) })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already"));
        gate.notify_one();
    }

    #[tokio::test]
    async fn allows_concurrent_imports_for_different_accounts() {
        let jobs = ImportJobs::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate2 = gate.clone();
        let _first = jobs
            .start(
                sample_job_for_account(ImportJobStatus::Queued, "account-1"),
                move || {
                    let gate2 = gate2.clone();
                    async move {
                        gate2.notified().await;
                        Ok(())
                    }
                },
            )
            .await
            .unwrap();
        let second = jobs
            .start(
                sample_job_for_account(ImportJobStatus::Queued, "account-2"),
                || async { Ok(()) },
            )
            .await
            .unwrap();
        assert_eq!(second.status, ImportJobStatus::Queued);
        gate.notify_one();
    }

    #[tokio::test]
    async fn completed_job_is_readable_and_frees_the_account_slot() {
        let jobs = ImportJobs::new();
        let job = jobs
            .start(sample_job(ImportJobStatus::Queued), || async { Ok(()) })
            .await
            .unwrap();

        for _ in 0..50 {
            if jobs
                .get(&job.id)
                .await
                .is_some_and(|job| job.status == ImportJobStatus::Completed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let fetched = jobs.get(&job.id).await.unwrap();
        assert_eq!(fetched.status, ImportJobStatus::Completed);
        assert!(fetched.started_at.is_some());
        assert!(fetched.finished_at.is_some());
        assert!(!jobs.account_has_active_import(&job.account_id).await);
    }

    #[tokio::test]
    async fn failed_job_records_failure_message() {
        let jobs = ImportJobs::new();
        let job = jobs
            .start(sample_job(ImportJobStatus::Queued), || async {
                anyhow::bail!("boom")
            })
            .await
            .unwrap();

        for _ in 0..50 {
            if jobs
                .get(&job.id)
                .await
                .is_some_and(|job| job.status.is_terminal())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let fetched = jobs.get(&job.id).await.unwrap();
        assert_eq!(fetched.status, ImportJobStatus::Failed);
        assert_eq!(fetched.failure_message.as_deref(), Some("boom"));
    }
}
