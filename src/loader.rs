//! Traits for module loading, unloading, and usage checking.
//!
//! These traits must be implemented by the kernel integration layer
//! (e.g., StarryOS) to bridge the on-demand framework with the actual
//! module loading infrastructure (e.g., `kmod-loader`, ELF parser).

/// Error returned by [`ModuleLoader::load`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// The `.ko` file was not found on disk.
    NotFound,
    /// The `.ko` file is not a valid module.
    InvalidModule,
    InitFailed(i32),
    Other,
}

/// Error returned by [`ModuleLoader::unload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadError {
    InUse,
    NotLoaded,
    ExitFailed,
    Other,
}

/// Handles loading and unloading of kernel modules.
///
/// Implementors bridge the on-demand framework with the actual module
/// loading infrastructure. For StarryOS, this would wrap
/// `kmod::init_module` and `kmod::delete_module`.
pub trait ModuleLoader: Send + Sync {
    /// Load a kernel module from disk and execute its init function.
    ///
    /// - `name`: Module name (e.g., `"procfs"`)
    /// - `ko_path`: Path to the `.ko` file (e.g., `"/root/modules/procfs.ko"`)
    ///
    /// Returns an opaque handle that will be passed to [`unload`](Self::unload)
    /// when the module should be removed. The handle can encode a module ID,
    /// a pointer, or any value meaningful to the loader.
    fn load(&self, name: &str, ko_path: &str) -> Result<u64, LoadError>;

    /// Unload a previously loaded module and execute its exit function.
    ///
    /// `handle` is the value returned by a prior successful [`load`](Self::load).
    fn unload(&self, handle: u64) -> Result<(), UnloadError>;
}

/// Checks whether a loaded module is actively in use and prepares it for
/// unloading.
///
/// Implementors provide module-specific usage tracking. For example, a procfs
/// checker might verify that no file descriptors are open under `/proc`.
///
/// # Contract
///
/// Both [`is_in_use`](Self::is_in_use) and [`prepare_unload`](Self::prepare_unload)
/// may be called while a spinlock is held. They **must not** block or perform
/// I/O.
pub trait UsageChecker: Send + Sync {
    /// Returns `true` if the module has active users that prevent unloading.
    fn is_in_use(&self) -> bool;

    /// Perform pre-unload cleanup (e.g., unmount a filesystem, unregister a
    /// syscall handler).
    ///
    /// Called only after [`is_in_use`](Self::is_in_use) returns `false`.
    /// Returns `Ok(())` if cleanup succeeded and unloading may proceed.
    fn prepare_unload(&self) -> Result<(), ()>;
}
