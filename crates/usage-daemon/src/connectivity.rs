use std::sync::{Arc, Mutex};

use chrono::Utc;
use usage_core::{Connectivity, ConnectivityStatus};

type StatusProbe = dyn Fn() -> ConnectivityStatus + Send + Sync;

/// Cheap, cloneable access to the daemon's machine-wide reachability state.
/// The platform probe is synchronous and local; it never sends network traffic.
#[derive(Clone)]
pub struct ConnectivityMonitor {
    probe: Arc<StatusProbe>,
    last: Arc<Mutex<Connectivity>>,
}

impl ConnectivityMonitor {
    pub fn system() -> Self {
        Self::with_probe(platform::status)
    }

    pub fn fixed(status: ConnectivityStatus) -> Self {
        Self::with_probe(move || status)
    }

    fn with_probe(probe: impl Fn() -> ConnectivityStatus + Send + Sync + 'static) -> Self {
        Self {
            probe: Arc::new(probe),
            last: Arc::new(Mutex::new(Connectivity::default())),
        }
    }

    pub fn current(&self) -> Connectivity {
        let status = (self.probe)();
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.status != status {
            last.status = status;
            last.changed_at = (status != ConnectivityStatus::Unknown).then(Utc::now);
        }
        last.clone()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{ffi::c_void, mem, ptr};

    use usage_core::ConnectivityStatus;

    type SCNetworkReachabilityRef = *const c_void;
    type SCNetworkReachabilityFlags = u32;

    const REACHABLE: SCNetworkReachabilityFlags = 1 << 1;
    const CONNECTION_REQUIRED: SCNetworkReachabilityFlags = 1 << 2;

    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCNetworkReachabilityCreateWithAddress(
            allocator: *const c_void,
            address: *const libc::sockaddr,
        ) -> SCNetworkReachabilityRef;
        fn SCNetworkReachabilityGetFlags(
            target: SCNetworkReachabilityRef,
            flags: *mut SCNetworkReachabilityFlags,
        ) -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
    }

    fn route_is_reachable(address: *const libc::sockaddr) -> Option<bool> {
        let target = unsafe { SCNetworkReachabilityCreateWithAddress(ptr::null(), address) };
        if target.is_null() {
            return None;
        }

        let mut flags = 0;
        let available = unsafe { SCNetworkReachabilityGetFlags(target, &raw mut flags) };
        unsafe { CFRelease(target) };
        if !available {
            return None;
        }

        Some(flags & REACHABLE != 0 && flags & CONNECTION_REQUIRED == 0)
    }

    pub fn status() -> ConnectivityStatus {
        // Zero addresses ask SystemConfiguration whether the machine has a
        // usable default route. They do not resolve a host or emit a packet.
        let mut ipv4 = unsafe { mem::zeroed::<libc::sockaddr_in>() };
        ipv4.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
        ipv4.sin_family = libc::AF_INET as u8;

        let mut ipv6 = unsafe { mem::zeroed::<libc::sockaddr_in6>() };
        ipv6.sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
        ipv6.sin6_family = libc::AF_INET6 as u8;

        let routes = [
            route_is_reachable((&raw const ipv4).cast::<libc::sockaddr>()),
            route_is_reachable((&raw const ipv6).cast::<libc::sockaddr>()),
        ];

        if routes.contains(&Some(true)) {
            ConnectivityStatus::Online
        } else if routes.contains(&Some(false)) {
            ConnectivityStatus::Offline
        } else {
            ConnectivityStatus::Unknown
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use usage_core::ConnectivityStatus;

    pub fn status() -> ConnectivityStatus {
        ConnectivityStatus::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_monitor_tracks_status_without_network_io() {
        let monitor = ConnectivityMonitor::fixed(ConnectivityStatus::Offline);
        let first = monitor.current();
        let second = monitor.current();

        assert_eq!(first.status, ConnectivityStatus::Offline);
        assert!(first.changed_at.is_some());
        assert_eq!(second.changed_at, first.changed_at);
    }
}
