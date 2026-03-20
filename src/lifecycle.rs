//! State machine, reference counting, and timestamp tracking for managed
//! kernel modules.
//!
//! This module defines the lifecycle [`State`] of a managed module, the RAII
//! [`ModuleGuard`] for reference counting, and the internal
//! [`ManagedModule`] that bundles all per-module bookkeeping.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::loader::UsageChecker;
use crate::trigger::Trigger;

/// Lifecycle state of a managed module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Registered,
    Loading,
    Active,
    Idle,
    Unloading,
    Unloaded,
}

/// User-provided descriptor for a managed module.
pub struct ModuleDesc {
    pub name: &'static str,
    pub ko_path: &'static str,
    pub idle_timeout_ticks: u64,

    /// Trigger that determines when this module should be loaded.
    pub trigger: Box<dyn Trigger>,
    pub usage: Option<Box<dyn UsageChecker>>,
}

/// Result of an `on_access` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessResult {
    NoMatch,
    Loaded,
    Loading,
    LoadFailed,
    Unavailable,
}

/// Read-only snapshot of a managed module's state.
#[derive(Debug, Clone)]
pub struct ModuleInfo {
    pub name: &'static str,
    pub state: State,
    /// Number of active references (guards).
    pub ref_count: usize,
    /// Last access timestamp (in ticks).
    pub last_access_ticks: u64,
    /// Timestamp when the module first became idle, if applicable.
    pub idle_since_ticks: Option<u64>,
}

// ---------------------------------------------------------------------------
// ModuleGuard
// ---------------------------------------------------------------------------

/// RAII guard that keeps a module's reference count elevated.
///
/// While this guard exists, the module will not be automatically unloaded
/// by the idle monitor. The reference count is decremented when the guard
/// is dropped.
pub struct ModuleGuard {
    ref_count: Arc<AtomicUsize>,
}

impl ModuleGuard {
    /// Create a guard for the given shared reference counter.
    ///
    /// The counter is incremented by the caller before constructing this
    /// guard; the guard is only responsible for decrementing on drop.
    pub(crate) fn new(ref_count: Arc<AtomicUsize>) -> Self {
        Self { ref_count }
    }
}

// Safety: ModuleGuard only contains an Arc<AtomicUsize>, both Send + Sync.
unsafe impl Send for ModuleGuard {}
unsafe impl Sync for ModuleGuard {}

impl Drop for ModuleGuard {
    fn drop(&mut self) {
        self.ref_count.fetch_sub(1, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// ManagedModule (crate-internal)
// ---------------------------------------------------------------------------

/// Internal bookkeeping for a registered module.
///
/// Combines the user-provided [`ModuleDesc`] with runtime state: lifecycle
/// phase, opaque loader handle, reference counting, and timestamps.
pub(crate) struct ManagedModule {
    pub(crate) desc: ModuleDesc,
    pub(crate) state: State,
    /// Opaque handle returned by [`ModuleLoader::load`].
    pub(crate) loaded_handle: Option<u64>,
    pub(crate) ref_count: Arc<AtomicUsize>,
    /// Tick at which the module was first observed idle by
    /// [`IdleMonitor::tick`](crate::monitor::IdleMonitor::tick).
    pub(crate) idle_since_ticks: Option<u64>,
    pub(crate) last_access_ticks: u64,
}

impl ManagedModule {
    /// Create a new `ManagedModule` from a descriptor.
    pub(crate) fn new(desc: ModuleDesc) -> Self {
        Self {
            desc,
            state: State::Registered,
            loaded_handle: None,
            ref_count: Arc::new(AtomicUsize::new(0)),
            idle_since_ticks: None,
            last_access_ticks: 0,
        }
    }

    /// Return the current reference count.
    pub(crate) fn ref_count_val(&self) -> usize {
        self.ref_count.load(Ordering::Acquire)
    }

    /// Mark as Active and reset idle tracking.
    pub(crate) fn touch(&mut self, now: u64) {
        self.state = State::Active;
        self.last_access_ticks = now;
        self.idle_since_ticks = None;
    }

    /// Snapshot for external inspection.
    pub(crate) fn info(&self) -> ModuleInfo {
        ModuleInfo {
            name: self.desc.name,
            state: self.state,
            ref_count: self.ref_count.load(Ordering::Relaxed),
            last_access_ticks: self.last_access_ticks,
            idle_since_ticks: self.idle_since_ticks,
        }
    }
}
