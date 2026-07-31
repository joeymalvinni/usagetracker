## Task 9 — Clippy dead-code fix

Wired `ImportJobs::account_has_active_import` into `DaemonRuntime::import_account_data` to reject duplicate imports before `start_import`, matching the existing `import_jobs.start` error message. Clippy `-D warnings` clean; all 11 `import_` tests pass.
