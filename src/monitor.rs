//! Idle monitor — periodic scanning and automatic unloading of idle modules.
//!
//! [`IdleMonitor`] encapsulates the three-phase `tick()` algorithm:
//!
//! 1. **Under lock**: transition `Active` → `Idle` when ref-count drops to
//!    zero; identify `Idle` modules whose timeout has expired; call
//!    `prepare_unload()`; mark as `Unloading`.
//! 2. **Without lock**: call `loader.unload()` for each candidate (may
//!    involve disk I/O).
//! 3. **Under lock**: finalize state to `Unloaded`.

use alloc::vec::Vec;
use spin::Mutex;

use crate::lifecycle::{ManagedModule, State};
use crate::loader::ModuleLoader;

// ---------------------------------------------------------------------------
// IdleMonitor
// ---------------------------------------------------------------------------

/// Scans managed modules and unloads those that have been idle for too long.
///
/// The monitor does **not** own the module list; it operates on a shared
/// `Mutex<Vec<ManagedModule>>` and a reference to the [`ModuleLoader`].
pub(crate) struct IdleMonitor;

impl IdleMonitor {
    /// Perform one tick of the idle monitor.
    ///
    /// `modules` is the shared module list owned by [`ModuleRegistry`].
    /// `loader` is the loader backend for performing the actual unload.
    /// `now` is the current time in abstract ticks.
    ///
    /// This method is safe to call concurrently with
    /// [`ModuleRegistry::on_access`] and [`ModuleRegistry::acquire`].
    pub(crate) fn tick<L: ModuleLoader>(
        modules: &Mutex<Vec<ManagedModule>>,
        loader: &L,
        now: u64,
    ) {
        // Phase 1: identify and prepare candidates (under lock).
        let to_unload = {
            let mut mods = modules.lock();
            let mut unload_list: Vec<(&'static str, u64)> = Vec::new();

            for m in mods.iter_mut() {
                match m.state {
                    State::Active => {
                        if m.ref_count_val() == 0 {
                            m.state = State::Idle;
                            if m.idle_since_ticks.is_none() {
                                m.idle_since_ticks = Some(now);
                            }
                        }
                    }
                    State::Idle => {
                        // Re-check ref count: someone may have acquired
                        // between ticks without calling on_access.
                        if m.ref_count_val() > 0 {
                            m.touch(now);
                            continue;
                        }

                        // Auto-unload disabled?
                        if m.desc.idle_timeout_ticks == 0 {
                            continue;
                        }

                        // Timeout not yet reached?
                        let idle_since = match m.idle_since_ticks {
                            Some(t) => t,
                            None => continue,
                        };
                        if now.saturating_sub(idle_since) < m.desc.idle_timeout_ticks {
                            continue;
                        }

                        // Check module-specific usage.
                        let can_unload = m
                            .desc
                            .usage
                            .as_ref()
                            .map(|u| !u.is_in_use())
                            .unwrap_or(true);
                        if !can_unload {
                            continue;
                        }

                        // Prepare for unload (must be non-blocking).
                        if let Some(checker) = m.desc.usage.as_ref() {
                            if checker.prepare_unload().is_err() {
                                continue;
                            }
                        }

                        m.state = State::Unloading;
                        if let Some(handle) = m.loaded_handle.take() {
                            unload_list.push((m.desc.name, handle));
                        }
                    }
                    _ => {}
                }
            }

            unload_list
        }; // lock released

        if to_unload.is_empty() {
            return;
        }

        // Phase 2: unload without lock (may involve I/O).
        let mut unload_results: Vec<(&'static str, u64, bool)> = Vec::new();
        for &(name, handle) in &to_unload {
            let ok = loader.unload(handle).is_ok();
            unload_results.push((name, handle, ok));
        }

        // Phase 3: finalize state transitions (under lock).
        {
            let mut mods = modules.lock();
            for &(name, handle, ok) in &unload_results {
                if let Some(m) = mods.iter_mut().find(|m| m.desc.name == name) {
                    if ok {
                        m.state = State::Unloaded;
                        m.idle_since_ticks = None;
                    } else {
                        // Unload failed: keep module active and restore handle so
                        // later ticks can retry safely.
                        m.state = State::Active;
                        m.loaded_handle = Some(handle);
                        m.touch(now);
                    }
                }
            }
        }
    }
}
