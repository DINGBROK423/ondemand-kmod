//! On-demand kernel module loading and unloading framework.

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
