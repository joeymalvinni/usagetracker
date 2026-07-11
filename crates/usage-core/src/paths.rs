use std::path::PathBuf;

pub const APP_DIR: &str = ".usagetracker";
pub const APP_HOME_ENV: &str = "USAGE_TRACKER_HOME";
pub const CONFIG_FILE: &str = "config.json";
pub const DB_FILE: &str = "usage.sqlite3";
pub const SOCKET_FILE: &str = "usage.sock";
pub const UI_DIR: &str = "ui";

pub fn default_app_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(APP_HOME_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    dirs::home_dir().map(|home| home.join(APP_DIR))
}

pub fn default_config_path() -> Option<PathBuf> {
    default_app_dir().map(|dir| dir.join(CONFIG_FILE))
}

pub fn default_db_path() -> Option<PathBuf> {
    default_app_dir().map(|dir| dir.join(DB_FILE))
}

pub fn default_socket_path() -> Option<PathBuf> {
    default_app_dir().map(|dir| dir.join(SOCKET_FILE))
}

pub fn default_ui_config_path() -> Option<PathBuf> {
    default_app_dir().map(|dir| dir.join(UI_DIR).join(CONFIG_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn app_home_environment_redirects_every_default_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os(APP_HOME_ENV);
        let root = std::env::temp_dir().join("usage-tracker-path-test");
        std::env::set_var(APP_HOME_ENV, &root);

        assert_eq!(default_app_dir().as_deref(), Some(root.as_path()));
        assert_eq!(default_config_path(), Some(root.join(CONFIG_FILE)));
        assert_eq!(default_db_path(), Some(root.join(DB_FILE)));
        assert_eq!(default_socket_path(), Some(root.join(SOCKET_FILE)));
        assert_eq!(
            default_ui_config_path(),
            Some(root.join(UI_DIR).join(CONFIG_FILE))
        );

        match previous {
            Some(value) => std::env::set_var(APP_HOME_ENV, value),
            None => std::env::remove_var(APP_HOME_ENV),
        }
    }
}
