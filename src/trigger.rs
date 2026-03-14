//! Access event matching for on-demand module loading.
//!
//! A [`Trigger`] determines whether an [`AccessEvent`] should cause a
//! particular module to be loaded. The framework ships with three built-in
//! trigger types; custom triggers can be created by implementing the trait.

/// An event that may trigger a module to be loaded.
///
/// Passed to [`Trigger::matches`] to determine whether a module should be
/// loaded in response to this event.
#[derive(Debug, Clone)]
pub enum AccessEvent<'a> {
    /// A file path was accessed (e.g., `open("/proc/meminfo")`).
    Path(&'a str),
    /// A system call was invoked by number.
    Syscall(usize),
    /// A device node was accessed (e.g., `/dev/null_blk0`).
    Device(&'a str),
}

/// Determines whether an [`AccessEvent`] should trigger loading of a module.
///
/// Implementations must be `Send + Sync` because the registry may be accessed
/// from multiple threads concurrently.
pub trait Trigger: Send + Sync {
    /// Returns `true` if `event` matches this trigger's criteria.
    fn matches(&self, event: &AccessEvent) -> bool;
}

// ---------------------------------------------------------------------------
// Built-in trigger implementations
// ---------------------------------------------------------------------------

/// Triggers when a file path matches a prefix with path-boundary awareness.
///
/// # Examples
///
/// `PathPrefixTrigger::new("/proc")` matches:
/// - `"/proc"` (exact match)
/// - `"/proc/meminfo"` (sub-path)
///
/// But does **not** match:
/// - `"/process"` (not at a path boundary)
/// - `"/dev/proc"` (different prefix)
pub struct PathPrefixTrigger {
    prefix: &'static str,
}

impl PathPrefixTrigger {
    /// Create a new path prefix trigger.
    pub const fn new(prefix: &'static str) -> Self {
        Self { prefix }
    }
}

impl Trigger for PathPrefixTrigger {
    fn matches(&self, event: &AccessEvent) -> bool {
        match event {
            AccessEvent::Path(path) => {
                *path == self.prefix
                    || (path.starts_with(self.prefix)
                        && path.as_bytes().get(self.prefix.len()) == Some(&b'/'))
            }
            _ => false,
        }
    }
}

/// Triggers when a specific system call number is invoked.
///
/// # Examples
///
/// ```ignore
/// // Trigger loading of the eBPF module when SYS_bpf is called
/// SyscallTrigger::new(321) // SYS_bpf on riscv64
/// ```
pub struct SyscallTrigger {
    sysno: usize,
}

impl SyscallTrigger {
    /// Create a new syscall trigger for the given syscall number.
    pub const fn new(sysno: usize) -> Self {
        Self { sysno }
    }
}

impl Trigger for SyscallTrigger {
    fn matches(&self, event: &AccessEvent) -> bool {
        matches!(event, AccessEvent::Syscall(n) if *n == self.sysno)
    }
}

/// Triggers when a device path matches a prefix.
///
/// Works identically to [`PathPrefixTrigger`] but only matches
/// [`AccessEvent::Device`] events.
pub struct DeviceTrigger {
    prefix: &'static str,
}

impl DeviceTrigger {
    /// Create a new device trigger.
    pub const fn new(prefix: &'static str) -> Self {
        Self { prefix }
    }
}

impl Trigger for DeviceTrigger {
    fn matches(&self, event: &AccessEvent) -> bool {
        match event {
            AccessEvent::Device(path) => {
                *path == self.prefix
                    || (path.starts_with(self.prefix)
                        && path.as_bytes().get(self.prefix.len()) == Some(&b'/'))
            }
            _ => false,
        }
    }
}
