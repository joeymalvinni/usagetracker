use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use tracing_subscriber::fmt::MakeWriter;

pub const LOG_FILE: &str = "usage-daemon.log";
const MAX_BYTES: u64 = 5 * 1_024 * 1_024;
const RETAINED_ARCHIVES: usize = 3;

#[derive(Clone)]
pub struct ManagedLogWriter {
    state: Arc<Mutex<LogState>>,
}

struct LogState {
    path: PathBuf,
    file: File,
    bytes: u64,
}

impl ManagedLogWriter {
    pub fn open(root: &Path) -> io::Result<Self> {
        reject_symlink(root)?;
        fs::create_dir_all(root)?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let path = root.join(LOG_FILE);
        reject_symlink(&path)?;
        let bytes = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut state = LogState {
            path: path.clone(),
            file: open_append(&path)?,
            bytes,
        };
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        if state.bytes >= MAX_BYTES {
            state.rotate()?;
        }
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl<'a> MakeWriter<'a> for ManagedLogWriter {
    type Writer = ManagedLogGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        ManagedLogGuard {
            state: self.state.lock().unwrap_or_else(|error| error.into_inner()),
        }
    }
}

pub struct ManagedLogGuard<'a> {
    state: MutexGuard<'a, LogState>,
}

impl Write for ManagedLogGuard<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.state.bytes > 0 && self.state.bytes.saturating_add(buffer.len() as u64) > MAX_BYTES
        {
            self.state.rotate()?;
        }
        let written = self.state.file.write(buffer)?;
        self.state.bytes = self.state.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.file.flush()
    }
}

impl LogState {
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let oldest = archive_path(&self.path, RETAINED_ARCHIVES);
        remove_if_present(&oldest)?;
        for index in (1..RETAINED_ARCHIVES).rev() {
            let source = archive_path(&self.path, index);
            if source.exists() {
                fs::rename(source, archive_path(&self.path, index + 1))?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, archive_path(&self.path, 1))?;
        }
        self.file = open_append(&self.path)?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        self.bytes = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    let mut archived = path.as_os_str().to_os_string();
    archived.push(format!(".{index}"));
    PathBuf::from(archived)
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing symlinked managed log path {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn opens_private_log_and_rotates_full_file() {
        let root = std::env::temp_dir().join(format!("usage-managed-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(LOG_FILE);
        fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();

        let _writer = ManagedLogWriter::open(&root).unwrap();

        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(
            fs::metadata(archive_path(&path, 1)).unwrap().len(),
            MAX_BYTES
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_symlinked_log_file() {
        let root = std::env::temp_dir().join(format!("usage-managed-log-{}", uuid::Uuid::new_v4()));
        let target = root.with_extension("target");
        fs::create_dir_all(&root).unwrap();
        fs::write(&target, "outside").unwrap();
        symlink(&target, root.join(LOG_FILE)).unwrap();

        let error = ManagedLogWriter::open(&root).err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(&target).unwrap(), "outside");
        fs::remove_dir_all(root).unwrap();
        fs::remove_file(target).unwrap();
    }
}
