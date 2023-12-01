#[cfg(target_os = "android")]
pub const SOCKET_PATH: &str = "/data/daemon/daemon.sock";
#[cfg(target_os = "android")]
pub const CONFIG_PATH: &str = "/data/daemon/config";
#[cfg(target_os = "android")]
pub const LOG_PATH: &str = "/data/daemon/daemon.log";

#[cfg(target_os = "linux")]
pub const SOCKET_PATH: &str = "/tmp/daemon.sock";
#[cfg(target_os = "linux")]
pub const CONFIG_PATH: &str = "/tmp/config";
#[cfg(target_os = "linux")]
pub const LOG_PATH: &str = "/tmp/daemon.log";

pub const RESTART_SEC: u64 = 1;
