//! On-demand kernel module loading and unloading framework.
//!
//! This `#![no_std]` crate provides a generic framework for managing kernel
//! modules that are **loaded on first access** and **automatically unloaded**
//! after a configurable idle timeout.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │              ModuleRegistry<L>                    │
//! │                                                  │
//! │  register(desc) ──► Registered                   │
//! │  on_access(event) ─► Loading ──► Active          │
//! │  acquire(name) ───► ModuleGuard (ref_count++)    │
//! │  tick(now) ───────► Idle ──► Unloading ──► Unloaded
//! │                                                  │
//! │  Trigger trait          ModuleLoader trait        │
//! │  (path/syscall/device)  (load .ko / unload)      │
//! └──────────────────────────────────────────────────┘
//! ```
//!
//! # State machine
//!
//! ```text
//! Registered ──on_access──► Loading ──success──► Active
//!     ▲                       │                  │    ▲
//!     │                     fail                 │    │
//!     │                       ▼         tick()   │  on_access
//!     │                    Unloaded ◄── Unloading │    │
//!     │                       ▲           ▲      ▼    │
//!     │                       │           └── Idle ───┘
//!     │                   on_access
//!     │                       │
//!     └───────────────────────┘ (re-load)
//! ```
//!
//! # Usage
//!
//! 1. Implement [`ModuleLoader`] to bridge with your kernel's module system.
//! 2. Create a [`ModuleRegistry`] with your loader.
//! 3. Call [`register`](ModuleRegistry::register) for each module, providing
//!    a [`Trigger`] that specifies when the module should be loaded.
//! 4. Call [`on_access`](ModuleRegistry::on_access) from syscall / VFS hooks
//!    to trigger loading automatically.
//! 5. Call [`tick`](ModuleRegistry::tick) periodically for automatic unloading
//!    of idle modules.

#![no_std]

extern crate alloc;

mod lifecycle;
mod loader;
mod monitor;
mod registry;
mod trigger;

pub use lifecycle::{AccessResult, ModuleDesc, ModuleGuard, ModuleInfo, State};
pub use loader::{LoadError, ModuleLoader, UnloadError, UsageChecker};
pub use registry::ModuleRegistry;
pub use trigger::{AccessEvent, DeviceTrigger, PathPrefixTrigger, SyscallTrigger, Trigger};
