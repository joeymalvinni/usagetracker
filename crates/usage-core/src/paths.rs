use std::path::PathBuf;

pub const APP_DIR: &str = ".usagetracker";
pub const CONFIG_FILE: &str = "config.json";
pub const DB_FILE: &str = "usage.sqlite3";
pub const SOCKET_FILE: &str = "usage.sock";
pub const UI_DIR: &str = "ui";

pub fn default_app_dir() -> Option<PathBuf> {
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
