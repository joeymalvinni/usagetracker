use std::{
    fs::{File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::Context;

const LOCK_SUFFIX: &str = ".lock";

/// Holds the per-socket process lock for the lifetime of a daemon instance.
pub(crate) struct InstanceGuard(File);

impl InstanceGuard {
    pub(crate) fn acquire(socket_path: &Path) -> anyhow::Result<Self> {
        let path = lock_path(socket_path);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to open daemon lock {}", path.display()))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Self(file));
        }

        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
            anyhow::bail!(
                "another daemon instance already owns socket {}",
                socket_path.display()
            );
        }
        Err(error).with_context(|| format!("failed to lock daemon instance {}", path.display()))
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(crate) fn lock_path(socket_path: &Path) -> PathBuf {
    let mut path = socket_path.as_os_str().to_owned();
    path.push(LOCK_SUFFIX);
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_socket() -> PathBuf {
        std::env::temp_dir().join(format!("usage-instance-{}.sock", uuid::Uuid::new_v4()))
    }

    #[test]
    fn lock_path_preserves_the_socket_name() {
        assert_eq!(
            lock_path(Path::new("/tmp/usage.sock")),
            Path::new("/tmp/usage.sock.lock")
        );
    }

    #[test]
    fn only_one_guard_can_own_a_socket() {
        let socket = test_socket();
        let first = InstanceGuard::acquire(&socket).unwrap();
        let error = InstanceGuard::acquire(&socket).err().unwrap();

        assert!(error.to_string().contains("already owns socket"));

        drop(first);
        InstanceGuard::acquire(&socket).unwrap();
        std::fs::remove_file(lock_path(&socket)).unwrap();
    }

    #[test]
    fn different_sockets_do_not_contend() {
        let first_socket = test_socket();
        let second_socket = test_socket();
        let first = InstanceGuard::acquire(&first_socket).unwrap();
        let second = InstanceGuard::acquire(&second_socket).unwrap();

        drop(first);
        drop(second);
        std::fs::remove_file(lock_path(&first_socket)).unwrap();
        std::fs::remove_file(lock_path(&second_socket)).unwrap();
    }
}
