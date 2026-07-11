use std::path::{Path, PathBuf};

/// Expands a leading `~` using the current user's home directory.
///
/// Only `~` and `~/...` are expanded. Other tildes (including `~user`) and
/// non-UTF-8 paths are returned unchanged.
pub(crate) fn expand_home_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_a_leading_home_component() {
        let home = dirs::home_dir().expect("test user has a home directory");
        assert_eq!(expand_home_path("~"), home);
        assert_eq!(expand_home_path("~/logs"), home.join("logs"));
        assert_eq!(
            expand_home_path("logs/~/file"),
            PathBuf::from("logs/~/file")
        );
        assert_eq!(
            expand_home_path("~someone/logs"),
            PathBuf::from("~someone/logs")
        );
    }
}
