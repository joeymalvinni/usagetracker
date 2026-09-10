//! Serializes UsageTracker's Keychain access and contains Security.framework hangs.

use std::{
    collections::{HashMap, VecDeque},
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    panic::{catch_unwind, AssertUnwindSafe},
    process::{Child, Command, Stdio},
    sync::{
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};

use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

#[cfg(target_os = "macos")]
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308; // Security.framework/SecBase.h

const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(60);
const CACHE_REVALIDATION_INTERVAL: Duration = Duration::from_secs(60);
// Periodically revalidate silently so another app's sign-in is discovered.
// A previously accepted value can still be used if revalidation needs UI;
// provider rejection explicitly invalidates it before attempting recovery.
// A small bounded queue prevents a hung Keychain backend from turning caller
// bursts into unbounded retained secrets while still absorbing normal polling.
const QUEUE_CAPACITY: usize = 8;
const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
const LOCK_FILE: &str = ".usagetracker/keychain.lock";

static BROKER: LazyLock<Broker> =
    LazyLock::new(|| Broker::new(QUEUE_CAPACITY, Arc::new(invoke_helper)));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Missing,
    AuthenticationFailed,
    InteractionRequired,
    QueueFull,
    QueueTimeout,
    HelperTimeout,
    HelperUnavailable,
    BackendRejected,
    Conflict,
    InvalidResponse,
}

/// Force a silent source check without discarding a value granted by Allow once.
pub(crate) fn revalidate_password(service: &str, account: &str) -> Result<(), Error> {
    invoke_once(Request::Revalidate {
        service: service.to_string(),
        account: account.to_string(),
    })
    .and_then(|response| match response {
        Response::Value { .. } => Ok(()),
        other => expect_ok(other),
    })
}

pub(crate) fn get_password(service: &str, account: &str) -> Result<String, Error> {
    let result = match invoke_once(Request::Get {
        service: service.to_string(),
        account: account.to_string(),
    }) {
        Ok(Response::Value { value }) => Ok(value),
        Ok(Response::Missing) => Err(Error::Missing),
        Ok(Response::Ok) => Err(Error::InvalidResponse),
        Ok(Response::Failed) => Err(Error::BackendRejected),
        Ok(Response::AuthenticationFailed) => Err(Error::AuthenticationFailed),
        Ok(Response::InteractionRequired) => Err(Error::InteractionRequired),
        Ok(Response::Conflict) => Err(Error::InvalidResponse),
        Err(error) => Err(error),
    };
    observe_error("get", &result);
    result
}

pub(crate) fn set_password_if_changed(
    service: &str,
    account: &str,
    password: &str,
) -> Result<(), Error> {
    let result = invoke_once(Request::SetIfChanged {
        service: service.to_string(),
        account: account.to_string(),
        password: password.to_string(),
    })
    .and_then(expect_ok);
    observe_error("set_if_changed", &result);
    result
}

/// Seeds a missing item without ever replacing credentials written by another process.
pub(crate) fn create_password_if_missing(
    service: &str,
    account: &str,
    password: &str,
) -> Result<(), Error> {
    let result = match invoke_once(Request::CreateIfMissing {
        service: service.to_string(),
        account: account.to_string(),
        password: password.to_string(),
    }) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Conflict) => Err(Error::Conflict),
        Ok(Response::Failed) => Err(Error::BackendRejected),
        Ok(_) => Err(Error::InvalidResponse),
        Err(error) => Err(error),
    };
    observe_error("create_if_missing", &result);
    result
}

pub(crate) fn compare_and_set_password(
    service: &str,
    account: &str,
    expected: &str,
    password: &str,
) -> Result<(), Error> {
    let result = match invoke_once(Request::CompareAndSet {
        service: service.to_string(),
        account: account.to_string(),
        expected: expected.to_string(),
        password: password.to_string(),
    }) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Conflict) => Err(Error::Conflict),
        Ok(Response::Missing) => Err(Error::Missing),
        Ok(Response::Failed) => Err(Error::BackendRejected),
        Ok(Response::AuthenticationFailed) => Err(Error::AuthenticationFailed),
        Ok(Response::InteractionRequired) => Err(Error::InteractionRequired),
        Ok(Response::Value { .. }) => Err(Error::InvalidResponse),
        Err(error) => Err(error),
    };
    observe_error("compare_and_set", &result);
    result
}

pub(crate) fn delete_password(service: &str, account: &str) -> Result<(), Error> {
    let result = invoke_once(Request::Delete {
        service: service.to_string(),
        account: account.to_string(),
    })
    .and_then(expect_ok);
    observe_error("delete", &result);
    result
}

/// Drops a brokered read without modifying the underlying Keychain item.
/// Call this after an external process may have replaced a credential.
pub(crate) fn invalidate_password_cache(service: &str, account: &str) -> Result<(), Error> {
    let result = invoke(Request::Invalidate {
        service: service.to_string(),
        account: account.to_string(),
    })
    .and_then(expect_ok);
    observe_error("invalidate", &result);
    result
}

fn observe_error<T>(operation: &str, result: &Result<T, Error>) {
    if let Err(error) = result {
        if *error != Error::Missing {
            tracing::debug!(operation, ?error, "Keychain operation failed");
        }
    }
}

fn expect_ok(response: Response) -> Result<(), Error> {
    match response {
        Response::Ok | Response::Missing => Ok(()),
        Response::Value { .. } => Err(Error::InvalidResponse),
        Response::Failed => Err(Error::BackendRejected),
        Response::AuthenticationFailed => Err(Error::AuthenticationFailed),
        Response::InteractionRequired => Err(Error::InteractionRequired),
        Response::Conflict => Err(Error::Conflict),
    }
}

/// Only an explicit credential-access action may use this operation. It reads
/// an existing item; it never opens login or retries a dismissed system prompt.
pub(crate) fn request_access(service: &str, account: &str) -> Result<(), Error> {
    match invoke_once(Request::Authorize {
        service: service.to_string(),
        account: account.to_string(),
    })? {
        Response::Value { .. } => Ok(()),
        Response::Missing => Err(Error::Missing),
        Response::Failed => Err(Error::BackendRejected),
        Response::AuthenticationFailed => Err(Error::AuthenticationFailed),
        Response::InteractionRequired => Err(Error::InteractionRequired),
        Response::Ok | Response::Conflict => Err(Error::InvalidResponse),
    }
}

fn invoke(request: Request) -> Result<Response, Error> {
    let timeout = if request.allows_interaction() {
        AUTHORIZATION_TIMEOUT
    } else {
        HELPER_TIMEOUT
    };
    BROKER.invoke(request, timeout)
}

fn invoke_once(request: Request) -> Result<Response, Error> {
    invoke_once_using(request, invoke)
}

fn invoke_once_using(
    request: Request,
    operation: impl FnOnce(Request) -> Result<Response, Error>,
) -> Result<Response, Error> {
    match operation(request)? {
        Response::AuthenticationFailed => Err(Error::AuthenticationFailed),
        Response::InteractionRequired => Err(Error::InteractionRequired),
        response => Ok(response),
    }
}

type Executor = Arc<dyn Fn(Request, Instant) -> Result<Response, Error> + Send + Sync>;

#[derive(Clone)]
struct Broker {
    requests: SyncSender<BrokerRequest>,
}

struct BrokerRequest {
    request: Request,
    deadline: Instant,
    replies: SyncSender<BrokerReply>,
}

enum BrokerReply {
    Started,
    Finished(Result<Response, Error>),
}

impl Broker {
    fn new(capacity: usize, executor: Executor) -> Self {
        Self::with_cache_interval(capacity, executor, CACHE_REVALIDATION_INTERVAL)
    }

    fn with_cache_interval(capacity: usize, executor: Executor, cache_interval: Duration) -> Self {
        let (requests, receiver) = mpsc::sync_channel(capacity);
        std::thread::Builder::new()
            .name("usage-keychain-broker".to_string())
            .spawn(move || run_broker(receiver, executor, cache_interval))
            .expect("failed to start Keychain broker");
        Self { requests }
    }

    fn invoke(&self, request: Request, timeout: Duration) -> Result<Response, Error> {
        let deadline = Instant::now() + timeout;
        let (replies, receiver) = mpsc::sync_channel(2);
        self.requests
            .try_send(BrokerRequest {
                request,
                deadline,
                replies,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => Error::QueueFull,
                TrySendError::Disconnected(_) => Error::HelperUnavailable,
            })?;

        let mut started = false;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(if started {
                    Error::HelperTimeout
                } else {
                    Error::QueueTimeout
                });
            };
            match receiver.recv_timeout(remaining) {
                Ok(BrokerReply::Started) => started = true,
                Ok(BrokerReply::Finished(result)) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(if started {
                        Error::HelperTimeout
                    } else {
                        Error::QueueTimeout
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::HelperUnavailable);
                }
            }
        }
    }
}

struct CachedPassword {
    response: Response,
    checked_at: Instant,
}

impl CachedPassword {
    fn current(&self, now: Instant, interval: Duration) -> Option<Response> {
        (now.duration_since(self.checked_at) < interval).then(|| self.response.clone())
    }
}

fn run_broker(receiver: Receiver<BrokerRequest>, executor: Executor, cache_interval: Duration) {
    let mut pending = VecDeque::new();
    let mut read_cache: HashMap<(String, String), CachedPassword> = HashMap::new();
    loop {
        let queued = match pending.pop_front() {
            Some(queued) => queued,
            None => match receiver.recv() {
                Ok(queued) => queued,
                Err(_) => break,
            },
        };
        if Instant::now() >= queued.deadline {
            let _ = queued
                .replies
                .send(BrokerReply::Finished(Err(Error::QueueTimeout)));
            continue;
        }
        if queued.replies.send(BrokerReply::Started).is_err() {
            continue;
        }
        let request = queued.request.clone();
        let cached = match &request {
            Request::Get { .. } => request.cache_key().and_then(|key| {
                read_cache.get(&key).and_then(|cached| cached.current(Instant::now(), cache_interval))
            }),
            Request::SetIfChanged { password, .. } => request.cache_key().and_then(|key| {
                read_cache
                    .get(&key)
                    .filter(|cached| matches!(&cached.response, Response::Value { value } if value == password))
                    .map(|_| Response::Ok)
            }),
            Request::Invalidate { .. } => Some(Response::Ok),
            Request::Authorize { .. }
            | Request::Revalidate { .. }
            | Request::CompareAndSet { .. }
            | Request::CreateIfMissing { .. }
            | Request::Delete { .. } => None,
        };
        let cache_hit = cached.is_some();
        let mut result = if let Some(response) = cached {
            Ok(response)
        } else {
            match catch_unwind(AssertUnwindSafe(|| {
                executor(queued.request, queued.deadline)
            })) {
                Ok(result) => result,
                Err(_) => {
                    tracing::error!("Keychain broker executor panicked; keeping broker available");
                    Err(Error::HelperUnavailable)
                }
            }
        };
        if !cache_hit || matches!(&request, Request::Invalidate { .. }) {
            // "Allow once" can leave usable credentials in memory even when a
            // later silent read requires permission. Reuse them until provider
            // authentication rejects them, at which point the caller invalidates.
            if matches!(&request, Request::Get { .. } | Request::Revalidate { .. })
                && matches!(
                    result,
                    Ok(Response::InteractionRequired | Response::AuthenticationFailed)
                )
            {
                if let Some(cached) = request.cache_key().and_then(|key| read_cache.get(&key)) {
                    if matches!(&cached.response, Response::Value { .. }) {
                        result = Ok(cached.response.clone());
                    }
                }
            }
            update_read_cache(&mut read_cache, &request, &result);
        }
        let _ = queued.replies.send(BrokerReply::Finished(result.clone()));
        let mut coalescing_allowed = true;
        while let Ok(candidate) = receiver.try_recv() {
            if coalescing_allowed
                && candidate.request.coalesces_with(&request)
                && Instant::now() < candidate.deadline
            {
                let _ = candidate.replies.send(BrokerReply::Started);
                let _ = candidate
                    .replies
                    .send(BrokerReply::Finished(result.clone()));
            } else {
                // Once any intervening operation is observed, all later work
                // must retain FIFO ordering. In particular, a read queued
                // after a write must never reuse the preceding read result.
                coalescing_allowed = false;
                pending.push_back(candidate);
            }
        }
    }
}

fn update_read_cache(
    cache: &mut HashMap<(String, String), CachedPassword>,
    request: &Request,
    result: &Result<Response, Error>,
) {
    let Some(key) = request.cache_key() else {
        return;
    };
    let response = match (request, result) {
        (
            Request::Get { .. } | Request::Revalidate { .. } | Request::Authorize { .. },
            Ok(
                response @ (Response::Value { .. }
                | Response::InteractionRequired
                | Response::AuthenticationFailed),
            ),
        ) => Some(response.clone()),
        (Request::SetIfChanged { password, .. }, Ok(Response::Ok))
        | (Request::CreateIfMissing { password, .. }, Ok(Response::Ok))
        | (Request::CompareAndSet { password, .. }, Ok(Response::Ok)) => Some(Response::Value {
            value: password.clone(),
        }),
        // A failed mutation leaves the actual Keychain state uncertain. Deletes
        // and missing reads must not preserve an older successful read.
        (
            Request::Get { .. } | Request::Revalidate { .. } | Request::Authorize { .. },
            Ok(Response::Missing),
        )
        | (Request::Invalidate { .. }, Ok(Response::Ok))
        | (Request::Delete { .. }, Ok(_)) => None,
        (Request::SetIfChanged { .. }, _)
        | (Request::CreateIfMissing { .. }, _)
        | (Request::CompareAndSet { .. }, _)
        | (Request::Invalidate { .. }, _)
        | (Request::Delete { .. }, _) => None,
        _ => return,
    };
    if let Some(response) = response {
        cache.insert(
            key,
            CachedPassword {
                response,
                checked_at: Instant::now(),
            },
        );
    } else {
        cache.remove(&key);
    }
}

fn invoke_helper(request: Request, deadline: Instant) -> Result<Response, Error> {
    let payload = serde_json::to_vec(&request).map_err(|_| Error::InvalidResponse)?;
    if payload.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(Error::InvalidResponse);
    }

    let mut command = Command::new(std::env::current_exe().map_err(|_| Error::HelperUnavailable)?);
    command.arg("--keychain-helper");
    invoke_helper_command(&mut command, payload, deadline)
}

fn invoke_helper_command(
    command: &mut Command,
    payload: Vec<u8>,
    deadline: Instant,
) -> Result<Response, Error> {
    let mut child = ChildGuard(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Error::HelperUnavailable)?,
    );
    let mut stdin = child.0.stdin.take().ok_or(Error::HelperUnavailable)?;
    // A helper can stop reading before the payload fits in the pipe. Keep the
    // broker thread free to enforce the same end-to-end deadline as the read.
    let writer = std::thread::spawn(move || stdin.write_all(&payload));

    let stdout = child.0.stdout.take().ok_or(Error::HelperUnavailable)?;
    let reader = std::thread::spawn(move || {
        let mut response = Vec::new();
        stdout
            .take(MAX_MESSAGE_BYTES + 1)
            .read_to_end(&mut response)
            .map(|_| response)
    });

    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::HelperTimeout)?;
    let status = match child.0.wait_timeout(remaining) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.0.kill();
            let _ = child.0.wait();
            let _ = writer.join();
            let _ = reader.join();
            return Err(Error::HelperTimeout);
        }
        Err(_) => {
            let _ = child.0.kill();
            let _ = child.0.wait();
            let _ = writer.join();
            let _ = reader.join();
            return Err(Error::HelperUnavailable);
        }
    };
    writer
        .join()
        .map_err(|_| Error::HelperUnavailable)?
        .map_err(|_| Error::HelperUnavailable)?;
    let response = reader
        .join()
        .map_err(|_| Error::HelperUnavailable)?
        .map_err(|_| Error::HelperUnavailable)?;
    if !status.success() {
        return Err(Error::HelperUnavailable);
    }
    if response.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(Error::InvalidResponse);
    }
    serde_json::from_slice(&response).map_err(|_| Error::InvalidResponse)
}

pub(crate) fn run_helper() -> anyhow::Result<()> {
    watch_parent();
    let mut payload = Vec::new();
    std::io::stdin()
        .take(MAX_MESSAGE_BYTES + 1)
        .read_to_end(&mut payload)?;
    anyhow::ensure!(
        payload.len() as u64 <= MAX_MESSAGE_BYTES,
        "Keychain request too large"
    );
    let request: Request = serde_json::from_slice(&payload)?;
    let _lock = KeychainLock::acquire()?;
    serde_json::to_writer(std::io::stdout(), &perform(request))?;
    Ok(())
}

fn watch_parent() {
    let parent = unsafe { libc::getppid() };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        if unsafe { libc::getppid() } != parent {
            unsafe { libc::_exit(1) };
        }
    });
}

fn perform(request: Request) -> Response {
    // This flag is process-wide, so apply it only inside the isolated helper.
    // Ordinary reads, writes, deletes, and browser-key reads all fail without UI.
    #[cfg(target_os = "macos")]
    if unsafe {
        security_framework_sys::keychain::SecKeychainSetUserInteractionAllowed(u8::from(
            request.allows_interaction(),
        ))
    } != 0
    {
        return Response::Failed;
    }
    match request {
        Request::Get { service, account }
        | Request::Revalidate { service, account }
        | Request::Authorize { service, account } => {
            match entry(&service, &account).and_then(|entry| entry.get_password()) {
                Ok(value) => Response::Value { value },
                Err(KeyringError::NoEntry) => Response::Missing,
                Err(error) => keyring_failure_response(&error),
            }
        }
        Request::SetIfChanged {
            service,
            account,
            password,
        } => write_password(&service, &account, &password, true),
        Request::CreateIfMissing {
            service,
            account,
            password,
        } => create_password(&service, &account, &password),
        Request::CompareAndSet {
            service,
            account,
            expected,
            password,
        } => {
            let entry = match entry(&service, &account) {
                Ok(entry) => entry,
                Err(error) => return keyring_failure_response(&error),
            };
            match entry.get_password() {
                Ok(current) if current == expected => match entry.set_password(&password) {
                    Ok(()) => Response::Ok,
                    Err(error) => keyring_failure_response(&error),
                },
                Ok(_) => Response::Conflict,
                Err(KeyringError::NoEntry) => Response::Missing,
                Err(error) => keyring_failure_response(&error),
            }
        }
        Request::Delete { service, account } => {
            match entry(&service, &account).and_then(|entry| entry.delete_credential()) {
                Ok(()) | Err(KeyringError::NoEntry) => Response::Ok,
                Err(error) => keyring_failure_response(&error),
            }
        }
        // Cache invalidation is handled by the broker and never needs to reach
        // the helper. Keep the protocol arm defensive if it is invoked directly.
        Request::Invalidate { .. } => Response::Ok,
    }
}

#[cfg(target_os = "macos")]
fn create_password(service: &str, account: &str, password: &str) -> Response {
    use security_framework::os::macos::keychain::{SecKeychain, SecPreferencesDomain};
    // SecKeychainAddGenericPassword is insert-only. A competing login/refresh
    // wins even if it happens after our initial missing-item read.
    match SecKeychain::default_for_domain(SecPreferencesDomain::User)
        .and_then(|keychain| keychain.add_generic_password(service, account, password.as_bytes()))
    {
        Ok(()) => Response::Ok,
        Err(error) if error.code() == security_framework_sys::base::errSecDuplicateItem => {
            Response::Conflict
        }
        Err(error) => keyring_failure_response(&KeyringError::PlatformFailure(Box::new(error))),
    }
}

#[cfg(not(target_os = "macos"))]
fn create_password(_service: &str, _account: &str, _password: &str) -> Response {
    // Managed Claude launches are macOS-only. Do not emulate insert-only with
    // a read followed by an unconditional write on another backend.
    Response::Failed
}

fn write_password(service: &str, account: &str, password: &str, only_if_changed: bool) -> Response {
    let entry = match entry(service, account) {
        Ok(entry) => entry,
        Err(error) => return keyring_failure_response(&error),
    };
    if only_if_changed {
        match entry.get_password() {
            Ok(current) if current == password => return Response::Ok,
            Ok(_) | Err(KeyringError::NoEntry) => {}
            Err(error) => return keyring_failure_response(&error),
        }
    }
    match entry.set_password(password) {
        Ok(()) => Response::Ok,
        Err(error) => keyring_failure_response(&error),
    }
}

fn keyring_failure_response(error: &KeyringError) -> Response {
    if is_interaction_required(error) {
        Response::InteractionRequired
    } else if is_authentication_failure(error) {
        Response::AuthenticationFailed
    } else {
        Response::Failed
    }
}

#[cfg(target_os = "macos")]
fn is_authentication_failure(error: &KeyringError) -> bool {
    let platform_error = match error {
        KeyringError::PlatformFailure(error) | KeyringError::NoStorageAccess(error) => error,
        _ => return false,
    };
    platform_error
        .downcast_ref::<security_framework::base::Error>()
        .is_some_and(|error| error.code() == security_framework_sys::base::errSecAuthFailed)
}

#[cfg(not(target_os = "macos"))]
fn is_authentication_failure(_error: &KeyringError) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn is_interaction_required(error: &KeyringError) -> bool {
    match error {
        KeyringError::PlatformFailure(error) | KeyringError::NoStorageAccess(error) => error
            .downcast_ref::<security_framework::base::Error>()
            .is_some_and(|error| error.code() == ERR_SEC_INTERACTION_NOT_ALLOWED),
        _ => false,
    }
}

#[cfg(not(target_os = "macos"))]
fn is_interaction_required(_error: &KeyringError) -> bool {
    false
}

fn entry(service: &str, account: &str) -> Result<Entry, KeyringError> {
    Entry::new(service, account)
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct KeychainLock(File);

impl KeychainLock {
    fn acquire() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory unavailable"))?;
        let path = home.join(LOCK_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        anyhow::ensure!(result == 0, "failed to acquire Keychain lock");
        Ok(Self(file))
    }
}

impl Drop for KeychainLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Request {
    Authorize {
        service: String,
        account: String,
    },
    Get {
        service: String,
        account: String,
    },
    Revalidate {
        service: String,
        account: String,
    },
    SetIfChanged {
        service: String,
        account: String,
        password: String,
    },
    CreateIfMissing {
        service: String,
        account: String,
        password: String,
    },
    CompareAndSet {
        service: String,
        account: String,
        expected: String,
        password: String,
    },
    Delete {
        service: String,
        account: String,
    },
    Invalidate {
        service: String,
        account: String,
    },
}

impl Request {
    fn allows_interaction(&self) -> bool {
        matches!(self, Self::Authorize { .. })
    }

    fn cache_key(&self) -> Option<(String, String)> {
        match self {
            Self::Get { service, account }
            | Self::Revalidate { service, account }
            | Self::Authorize { service, account }
            | Self::SetIfChanged {
                service, account, ..
            }
            | Self::CreateIfMissing {
                service, account, ..
            }
            | Self::CompareAndSet {
                service, account, ..
            }
            | Self::Delete { service, account }
            | Self::Invalidate { service, account } => Some((service.clone(), account.clone())),
        }
    }

    fn coalesces_with(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (
                Self::Get { service, account },
                Self::Get {
                    service: other_service,
                    account: other_account,
                }
            ) if service == other_service && account == other_account
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Ok,
    Value { value: String },
    Missing,
    AuthenticationFailed,
    InteractionRequired,
    Failed,
    Conflict,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Condvar, Mutex,
    };

    fn request() -> Request {
        Request::Get {
            service: "test".to_string(),
            account: "account".to_string(),
        }
    }

    fn wait_until(gate: &(Mutex<bool>, Condvar)) {
        let (state, changed) = gate;
        let mut ready = state.lock().unwrap();
        while !*ready {
            ready = changed.wait(ready).unwrap();
        }
    }

    fn open(gate: &(Mutex<bool>, Condvar)) {
        let (state, changed) = gate;
        *state.lock().unwrap() = true;
        changed.notify_all();
    }

    #[test]
    fn helper_protocol_round_trips_a_conditional_write() {
        let request = Request::SetIfChanged {
            service: "cache".to_string(),
            account: "provider".to_string(),
            password: "secret".to_string(),
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&encoded).unwrap();

        assert!(matches!(
            decoded,
            Request::SetIfChanged {
                service,
                account,
                password
            } if service == "cache" && account == "provider" && password == "secret"
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classifies_macos_authentication_failure_without_conflating_cancel() {
        let authentication =
            KeyringError::PlatformFailure(Box::new(security_framework::base::Error::from_code(
                security_framework_sys::base::errSecAuthFailed,
            )));
        let blocked = KeyringError::PlatformFailure(Box::new(
            security_framework::base::Error::from_code(ERR_SEC_INTERACTION_NOT_ALLOWED),
        ));
        assert!(matches!(
            keyring_failure_response(&blocked),
            Response::InteractionRequired
        ));
        let canceled = KeyringError::PlatformFailure(Box::new(
            security_framework::base::Error::from_code(-128),
        ));

        assert!(is_authentication_failure(&authentication));
        assert!(!is_authentication_failure(&canceled));
        assert!(matches!(
            keyring_failure_response(&authentication),
            Response::AuthenticationFailed
        ));
        assert!(matches!(
            keyring_failure_response(&canceled),
            Response::Failed
        ));
    }

    #[test]
    fn denied_or_canceled_access_never_reprompts() {
        for response in [
            Response::AuthenticationFailed,
            Response::InteractionRequired,
            Response::Failed,
        ] {
            let calls = AtomicUsize::new(0);
            let _ = invoke_once_using(
                Request::Authorize {
                    service: "test".into(),
                    account: "account".into(),
                },
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(response)
                },
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn explicit_access_bypasses_cached_denial_and_primes_silent_reads() {
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_calls = calls.clone();
        let broker = Broker::new(
            4,
            Arc::new(move |request, _| {
                executor_calls.fetch_add(1, Ordering::SeqCst);
                if request.allows_interaction() {
                    Ok(Response::Value {
                        value: "accepted".into(),
                    })
                } else {
                    Ok(Response::InteractionRequired)
                }
            }),
        );
        for _ in 0..3 {
            assert!(matches!(
                broker.invoke(request(), Duration::from_secs(1)),
                Ok(Response::InteractionRequired)
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let authorize = Request::Authorize {
            service: "test".into(),
            account: "account".into(),
        };
        assert!(!authorize.coalesces_with(&request()));
        assert!(!request().coalesces_with(&authorize));
        assert!(!request().allows_interaction());
        assert!(matches!(
            broker.invoke(authorize, Duration::from_secs(1)),
            Ok(Response::Value { .. })
        ));
        assert!(
            matches!(broker.invoke(request(), Duration::from_secs(1)), Ok(Response::Value { value }) if value == "accepted")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn silent_revalidation_observes_external_rotation_and_removal() {
        let responses = Arc::new(Mutex::new(VecDeque::from([
            Response::Value {
                value: "old".into(),
            },
            Response::Value {
                value: "new".into(),
            },
            Response::Missing,
            Response::InteractionRequired,
        ])));
        let broker = Broker::with_cache_interval(
            4,
            Arc::new(move |request, _| {
                assert!(!request.allows_interaction());
                Ok(responses.lock().unwrap().pop_front().unwrap())
            }),
            Duration::ZERO,
        );
        assert!(
            matches!(broker.invoke(request(), Duration::from_secs(1)), Ok(Response::Value { value }) if value == "old")
        );
        assert!(
            matches!(broker.invoke(request(), Duration::from_secs(1)), Ok(Response::Value { value }) if value == "new")
        );
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Missing)
        ));
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::InteractionRequired)
        ));
    }

    #[test]
    fn accepted_credentials_survive_silent_permission_failure_until_invalidated() {
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_calls = calls.clone();
        let broker = Broker::with_cache_interval(
            4,
            Arc::new(move |request, _| {
                assert!(!request.allows_interaction());
                Ok(if executor_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Response::Value {
                        value: "accepted-once".into(),
                    }
                } else {
                    Response::InteractionRequired
                })
            }),
            Duration::ZERO,
        );
        for _ in 0..2 {
            assert!(
                matches!(broker.invoke(request(), Duration::from_secs(1)), Ok(Response::Value { value }) if value == "accepted-once")
            );
        }
        broker
            .invoke(
                Request::Invalidate {
                    service: "test".into(),
                    account: "account".into(),
                },
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::InteractionRequired)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn allow_once_survives_refresh_revalidation_and_silent_auth_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let broker = Broker::new(
            4,
            Arc::new(move |request, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(match request {
                    Request::Authorize { .. } => Response::Value {
                        value: "allowed-token".into(),
                    },
                    Request::Revalidate { .. } => Response::AuthenticationFailed,
                    _ => panic!("subsequent reads should use the accepted value"),
                })
            }),
        );
        for request in [
            Request::Authorize {
                service: "test".into(),
                account: "account".into(),
            },
            Request::Revalidate {
                service: "test".into(),
                account: "account".into(),
            },
            request(),
        ] {
            assert!(matches!(broker.invoke(request, Duration::from_secs(1)),
                Ok(Response::Value { value }) if value == "allowed-token"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn helper_deadline_contains_a_child_that_never_reads_stdin() {
        let mut command = Command::new("sleep");
        command.arg("5");
        let started = Instant::now();

        let error = invoke_helper_command(
            &mut command,
            vec![b'x'; MAX_MESSAGE_BYTES as usize],
            Instant::now() + Duration::from_millis(50),
        )
        .unwrap_err();

        assert_eq!(error, Error::HelperTimeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn broker_rejects_work_when_its_queue_is_full() {
        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let executor_started = started.clone();
        let executor_release = release.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |_, _| {
                open(&executor_started);
                wait_until(&executor_release);
                Ok(Response::Ok)
            }),
        );
        let first_broker = broker.clone();
        let first =
            std::thread::spawn(move || first_broker.invoke(request(), Duration::from_secs(1)));
        wait_until(&started);
        let (queued_replies, _queued_receiver) = mpsc::sync_channel(2);
        broker
            .requests
            .try_send(BrokerRequest {
                request: request(),
                deadline: Instant::now() + Duration::from_secs(1),
                replies: queued_replies,
            })
            .unwrap();

        assert_eq!(
            broker
                .invoke(request(), Duration::from_secs(1))
                .unwrap_err(),
            Error::QueueFull
        );

        open(&release);
        assert!(matches!(first.join().unwrap(), Ok(Response::Ok)));
    }

    #[test]
    fn broker_deadline_includes_time_spent_queued() {
        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_started = started.clone();
        let executor_release = release.clone();
        let executor_calls = calls.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |_, _| {
                executor_calls.fetch_add(1, Ordering::SeqCst);
                open(&executor_started);
                wait_until(&executor_release);
                Ok(Response::Ok)
            }),
        );
        let first_broker = broker.clone();
        let first =
            std::thread::spawn(move || first_broker.invoke(request(), Duration::from_secs(1)));
        wait_until(&started);

        assert_eq!(
            broker
                .invoke(request(), Duration::from_millis(20))
                .unwrap_err(),
            Error::QueueTimeout
        );

        open(&release);
        assert!(matches!(first.join().unwrap(), Ok(Response::Ok)));
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Ok)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn broker_remains_available_after_executor_panics() {
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_calls = calls.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |_, _| {
                if executor_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("simulated helper panic");
                }
                Ok(Response::Ok)
            }),
        );

        assert_eq!(
            broker
                .invoke(request(), Duration::from_secs(1))
                .unwrap_err(),
            Error::HelperUnavailable
        );
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Ok)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn insert_conflict_reloads_the_winning_credential_instead_of_using_cached_data() {
        let value = Arc::new(Mutex::new("A".to_string()));
        let executor_value = value.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |request, _| match request {
                Request::Get { .. } => Ok(Response::Value {
                    value: executor_value.lock().unwrap().clone(),
                }),
                Request::CreateIfMissing { .. } => Ok(Response::Conflict),
                _ => panic!("unexpected request"),
            }),
        );
        assert!(
            matches!(broker.invoke(request(), Duration::from_secs(1)), Ok(Response::Value { value }) if value == "A")
        );
        // Another writer replaces the real item while the broker still caches A.
        *value.lock().unwrap() = "B".to_string();
        let Request::Get { service, account } = request() else {
            unreachable!()
        };
        assert!(matches!(
            broker.invoke(
                Request::CreateIfMissing {
                    service,
                    account,
                    password: "A".to_string()
                },
                Duration::from_secs(1)
            ),
            Ok(Response::Conflict)
        ));
        assert!(
            matches!(broker.invoke(request(), Duration::from_secs(1)), Ok(Response::Value { value }) if value == "B")
        );
        assert_eq!(*value.lock().unwrap(), "B");
    }

    #[test]
    fn broker_coalesces_successful_reads_between_revalidations() {
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_calls = calls.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |_, _| {
                executor_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Response::Value {
                    value: "secret".to_string(),
                })
            }),
        );

        for _ in 0..100 {
            assert!(matches!(
                broker.invoke(request(), Duration::from_secs(1)),
                Ok(Response::Value { value }) if value == "secret"
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn broker_invalidation_reloads_an_externally_changed_value() {
        let value = Arc::new(Mutex::new("old".to_string()));
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_value = value.clone();
        let executor_calls = calls.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |_, _| {
                executor_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Response::Value {
                    value: executor_value.lock().unwrap().clone(),
                })
            }),
        );

        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Value { value }) if value == "old"
        ));
        *value.lock().unwrap() = "new".to_string();
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Value { value }) if value == "old"
        ));
        assert!(matches!(
            broker.invoke(
                Request::Invalidate {
                    service: "test".to_string(),
                    account: "account".to_string(),
                },
                Duration::from_secs(1),
            ),
            Ok(Response::Ok)
        ));
        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Value { value }) if value == "new"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn broker_uses_cached_value_to_skip_an_unchanged_write() {
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_calls = calls.clone();
        let broker = Broker::new(
            1,
            Arc::new(move |_, _| {
                executor_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Response::Value {
                    value: "secret".to_string(),
                })
            }),
        );

        assert!(matches!(
            broker.invoke(request(), Duration::from_secs(1)),
            Ok(Response::Value { value }) if value == "secret"
        ));
        assert!(matches!(
            broker.invoke(
                Request::SetIfChanged {
                    service: "test".to_string(),
                    account: "account".to_string(),
                    password: "secret".to_string(),
                },
                Duration::from_secs(1),
            ),
            Ok(Response::Ok)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn broker_never_coalesces_a_read_across_an_intervening_write() {
        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let value = Arc::new(Mutex::new("old".to_string()));
        let calls = Arc::new(AtomicUsize::new(0));
        let executor_started = started.clone();
        let executor_release = release.clone();
        let executor_value = value.clone();
        let executor_calls = calls.clone();
        let broker = Broker::new(
            4,
            Arc::new(move |request, _| {
                let call = executor_calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    open(&executor_started);
                    wait_until(&executor_release);
                }
                match request {
                    Request::Get { .. } => Ok(Response::Value {
                        value: executor_value.lock().unwrap().clone(),
                    }),
                    Request::SetIfChanged { password, .. } => {
                        *executor_value.lock().unwrap() = password;
                        Ok(Response::Ok)
                    }
                    _ => Ok(Response::Failed),
                }
            }),
        );
        let first_broker = broker.clone();
        let first =
            std::thread::spawn(move || first_broker.invoke(request(), Duration::from_secs(1)));
        wait_until(&started);

        let (write_replies, write_receiver) = mpsc::sync_channel(2);
        broker
            .requests
            .try_send(BrokerRequest {
                request: Request::SetIfChanged {
                    service: "test".to_string(),
                    account: "account".to_string(),
                    password: "new".to_string(),
                },
                deadline: Instant::now() + Duration::from_secs(1),
                replies: write_replies,
            })
            .unwrap();
        let (read_replies, read_receiver) = mpsc::sync_channel(2);
        broker
            .requests
            .try_send(BrokerRequest {
                request: request(),
                deadline: Instant::now() + Duration::from_secs(1),
                replies: read_replies,
            })
            .unwrap();
        open(&release);

        assert!(matches!(
            first.join().unwrap(),
            Ok(Response::Value { value }) if value == "old"
        ));
        assert!(matches!(
            write_receiver.recv().unwrap(),
            BrokerReply::Started
        ));
        assert!(matches!(
            write_receiver.recv().unwrap(),
            BrokerReply::Finished(Ok(Response::Ok))
        ));
        assert!(matches!(
            read_receiver.recv().unwrap(),
            BrokerReply::Started
        ));
        assert!(matches!(
            read_receiver.recv().unwrap(),
            BrokerReply::Finished(Ok(Response::Value { value })) if value == "new"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
