//! Module registry — the core data structure for on-demand module management.
//!
//! [`ModuleRegistry`] owns the module list and loader, and exposes the
//! primary API: `register`, `on_access`, `acquire`, `force_unload`,
//! `state_of`, `list_modules`, and `tick`.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;
use spin::Mutex;

use crate::lifecycle::{
    AccessResult, ManagedModule, ModuleDesc, ModuleGuard, ModuleInfo, State,
};
use crate::loader::{ModuleLoader, UnloadError};
use crate::monitor::IdleMonitor;
use crate::trigger::AccessEvent;

// ---------------------------------------------------------------------------
// Internal helper
// ---------------------------------------------------------------------------

/// Action to perform after releasing the lock in `on_access`.
enum LoadAction {
    Load {
        name: &'static str,
        ko_path: &'static str,
    },
}

// ---------------------------------------------------------------------------
// ModuleRegistry
// ---------------------------------------------------------------------------

/// The central registry for on-demand kernel modules
pub struct ModuleRegistry<L: ModuleLoader> {
    pub(crate) modules: Mutex<Vec<ManagedModule>>,
    pub(crate) loader: L,
}

impl<L: ModuleLoader> ModuleRegistry<L> {
    /// Create a new registry with the given loader backend.
    pub const fn new(loader: L) -> Self {
        Self {
            modules: Mutex::new(Vec::new()),
            loader,
        }
    }

    /// Register a module for on-demand loading.
    pub fn register(&self, desc: ModuleDesc) -> bool {
        let mut modules = self.modules.lock();
        if modules.iter().any(|m| m.desc.name == desc.name) {
            return false;
        }
        modules.push(ManagedModule::new(desc));
        true
    }

    /// Notify the registry of an access event.
    ///
    /// If a registered module's trigger matches `event` and the module is
    /// not yet loaded, it will be loaded automatically. If the module is
    /// already loaded, this is a fast no-op that resets the idle timer.
    ///
    /// `now` is the current time in abstract ticks (same unit as
    /// [`ModuleDesc::idle_timeout_ticks`]).
    ///
    /// # Concurrency
    ///
    /// If another thread is currently loading the same module, this method
    /// returns [`AccessResult::Loading`]. The caller should yield (e.g.,
    /// `axtask::yield_now()`) and retry.
    pub fn on_access(&self, event: &AccessEvent, now: u64) -> AccessResult {
        // Fast path: lock, check triggers and state, release.
        let action = {
            let mut modules = self.modules.lock();
            let m = match modules.iter_mut().find(|m| m.desc.trigger.matches(event)) {
                Some(m) => m,
                None => return AccessResult::NoMatch,
            };

            match m.state {
                State::Active | State::Idle => {
                    m.touch(now);
                    return AccessResult::Loaded;
                }
                State::Registered | State::Unloaded => {
                    m.state = State::Loading;
                    LoadAction::Load {
                        name: m.desc.name,
                        ko_path: m.desc.ko_path,
                    }
                }
                State::Loading => return AccessResult::Loading,
                State::Unloading => return AccessResult::Unavailable,
            }
        }; // lock released

        // Slow path: load the module without holding the lock.
        let LoadAction::Load { name, ko_path } = action;
        let result = self.loader.load(name, ko_path);

        // Re-lock and update state.
        let mut modules = self.modules.lock();
        let m = modules
            .iter_mut()
            .find(|m| m.desc.name == name)
            .expect("module disappeared from registry during loading");

        match result {
            Ok(handle) => {
                m.loaded_handle = Some(handle);
                m.touch(now);
                AccessResult::Loaded
            }
            Err(_) => {
                m.state = State::Unloaded;
                AccessResult::LoadFailed
            }
        }
    }

    /// Acquire a reference to a loaded module, preventing automatic unloading.
    ///
    /// Returns `Some(ModuleGuard)` if the module is currently loaded
    /// ([`Active`](State::Active) or [`Idle`](State::Idle)).
    /// Returns `None` if the module is not loaded or does not exist.
    ///
    /// The returned [`ModuleGuard`] decrements the reference count when
    /// dropped, allowing the module to become eligible for unloading again.
    pub fn acquire(&self, name: &str, now: u64) -> Option<ModuleGuard> {
        let mut modules = self.modules.lock();
        let m = modules.iter_mut().find(|m| m.desc.name == name)?;

        match m.state {
            State::Active | State::Idle => {
                m.ref_count.fetch_add(1, Ordering::Acquire);
                m.touch(now);
                Some(ModuleGuard::new(Arc::clone(&m.ref_count)))
            }
            _ => None,
        }
    }

    /// Periodic tick — delegates to [`IdleMonitor::tick`].
    ///
    /// Should be called periodically by the kernel (e.g., from a timer
    /// callback). `now` is the current time in the same tick unit as
    /// [`ModuleDesc::idle_timeout_ticks`].
    pub fn tick(&self, now: u64) {
        IdleMonitor::tick(&self.modules, &self.loader, now);
    }

    /// Forcefully unload a module by name.
    ///
    /// Returns an error if the module is not loaded, has active references,
    /// or is busy (loading/unloading).
    pub fn force_unload(&self, name: &str) -> Result<(), UnloadError> {
        let handle = {
            let mut modules = self.modules.lock();
            let m = modules
                .iter_mut()
                .find(|m| m.desc.name == name)
                .ok_or(UnloadError::NotLoaded)?;

            match m.state {
                State::Active | State::Idle => {
                    if m.ref_count_val() > 0 {
                        return Err(UnloadError::InUse);
                    }
                    if let Some(checker) = m.desc.usage.as_ref() {
                        if checker.is_in_use() {
                            return Err(UnloadError::InUse);
                        }
                        let _ = checker.prepare_unload();
                    }
                    m.state = State::Unloading;
                    m.loaded_handle.take().ok_or(UnloadError::NotLoaded)?
                }
                State::Registered | State::Unloaded => {
                    return Err(UnloadError::NotLoaded);
                }
                State::Loading | State::Unloading => {
                    return Err(UnloadError::Other);
                }
            }
        }; // lock released

        self.loader.unload(handle)?;

        {
            let mut modules = self.modules.lock();
            if let Some(m) = modules.iter_mut().find(|m| m.desc.name == name) {
                m.state = State::Unloaded;
                m.idle_since_ticks = None;
            }
        }

        Ok(())
    }

    /// Query the current state of a module by name.
    pub fn state_of(&self, name: &str) -> Option<State> {
        let modules = self.modules.lock();
        modules
            .iter()
            .find(|m| m.desc.name == name)
            .map(|m| m.state)
    }

    /// Return a snapshot of all managed modules.
    pub fn list_modules(&self) -> Vec<ModuleInfo> {
        let modules = self.modules.lock();
        modules.iter().map(|m| m.info()).collect()
    }
}
