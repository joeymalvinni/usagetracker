//! Serializes UsageTracker's Keychain access and contains Security.framework hangs.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

const HELPER_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
const LOCK_FILE: &str = ".usagetracker/keychain.lock";

static LOCAL_ACCESS: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Missing,
    Timeout,
    Unavailable,
}

pub(crate) fn get_password(service: &str, account: &str) -> Result<String, Error> {
    match invoke(Request::Get {
        service: service.to_string(),
        account: account.to_string(),
    })? {
        Response::Value { value } => Ok(value),
        Response::Missing => Err(Error::Missing),
        Response::Ok | Response::Failed => Err(unavailable()),
    }
}

pub(crate) fn set_password(service: &str, account: &str, password: &str) -> Result<(), Error> {
    expect_ok(invoke(Request::Set {
        service: service.to_string(),
        account: account.to_string(),
        password: password.to_string(),
    })?)
}

pub(crate) fn set_password_if_changed(
    service: &str,
    account: &str,
    password: &str,
) -> Result<(), Error> {
    expect_ok(invoke(Request::SetIfChanged {
        service: service.to_string(),
        account: account.to_string(),
        password: password.to_string(),
    })?)
}

pub(crate) fn delete_password(service: &str, account: &str) -> Result<(), Error> {
    expect_ok(invoke(Request::Delete {
        service: service.to_string(),
        account: account.to_string(),
    })?)
}

fn expect_ok(response: Response) -> Result<(), Error> {
    match response {
        Response::Ok | Response::Missing => Ok(()),
        Response::Value { .. } | Response::Failed => Err(unavailable()),
    }
}

fn invoke(request: Request) -> Result<Response, Error> {
    let _access = LOCAL_ACCESS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let payload = serde_json::to_vec(&request).map_err(|_| unavailable())?;
    if payload.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(unavailable());
    }

    let mut child = ChildGuard(
        Command::new(std::env::current_exe().map_err(|_| unavailable())?)
            .arg("--keychain-helper")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| unavailable())?,
    );
    child
        .0
        .stdin
        .take()
        .ok_or_else(unavailable)?
        .write_all(&payload)
        .map_err(|_| unavailable())?;

    let stdout = child.0.stdout.take().ok_or_else(unavailable)?;
    let reader = std::thread::spawn(move || {
        let mut response = Vec::new();
        stdout
            .take(MAX_MESSAGE_BYTES + 1)
            .read_to_end(&mut response)
            .map(|_| response)
    });

    let status = match child.0.wait_timeout(HELPER_TIMEOUT) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.0.kill();
            let _ = child.0.wait();
            let _ = reader.join();
            return Err(Error::Timeout);
        }
        Err(_) => return Err(unavailable()),
    };
    let response = reader
        .join()
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    if !status.success() || response.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(unavailable());
    }
    serde_json::from_slice(&response).map_err(|_| unavailable())
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
    match request {
        Request::Get { service, account } => {
            match entry(&service, &account).and_then(|entry| entry.get_password()) {
                Ok(value) => Response::Value { value },
                Err(KeyringError::NoEntry) => Response::Missing,
                Err(_) => Response::Failed,
            }
        }
        Request::Set {
            service,
            account,
            password,
        } => write_password(&service, &account, &password, false),
        Request::SetIfChanged {
            service,
            account,
            password,
        } => write_password(&service, &account, &password, true),
        Request::Delete { service, account } => {
            match entry(&service, &account).and_then(|entry| entry.delete_credential()) {
                Ok(()) | Err(KeyringError::NoEntry) => Response::Ok,
                Err(_) => Response::Failed,
            }
        }
    }
}

fn write_password(service: &str, account: &str, password: &str, only_if_changed: bool) -> Response {
    let entry = match entry(service, account) {
        Ok(entry) => entry,
        Err(_) => return Response::Failed,
    };
    if only_if_changed {
        match entry.get_password() {
            Ok(current) if current == password => return Response::Ok,
            Ok(_) | Err(KeyringError::NoEntry) => {}
            Err(_) => return Response::Failed,
        }
    }
    match entry.set_password(password) {
        Ok(()) => Response::Ok,
        Err(_) => Response::Failed,
    }
}

fn entry(service: &str, account: &str) -> Result<Entry, KeyringError> {
    Entry::new(service, account)
}

fn unavailable() -> Error {
    Error::Unavailable
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Request {
    Get {
        service: String,
        account: String,
    },
    Set {
        service: String,
        account: String,
        password: String,
    },
    SetIfChanged {
        service: String,
        account: String,
        password: String,
    },
    Delete {
        service: String,
        account: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Ok,
    Value { value: String },
    Missing,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
