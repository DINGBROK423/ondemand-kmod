# ondemand-kmod

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

A generic `#![no_std]` Rust framework for **on-demand kernel module loading and automatic idle unloading**.

`ondemand-kmod` decouples *"when to load"* (policy) from *"how to load"* (mechanism). It provides a lightweight registry, trigger system, lifecycle state machine, and idle monitor so that kernel modules can be loaded lazily on first access and automatically reclaimed after a configurable timeout.

> This crate is designed to be embedded into unikernel or monolithic kernel projects. It powers the on-demand module subsystem in [**StarryOS**](https://github.com/Starry-OS/StarryOS).

---

## Table of Contents

- [Features](#features)
- [Architecture](#architecture)
  - [State Machine](#state-machine)
  - [Three-Phase Unload](#three-phase-unload)
- [Quick Start](#quick-start)
- [Integration Example: StarryOS](#integration-example-starryos)
- [Tests](#tests)
- [License](#license)

---

## Features

- **`#![no_std]` & `alloc` only** — runs in bare-metal environments.
- **Minimal dependencies** — only relies on `spin` for `Mutex`.
- **Pluggable triggers** — load modules by path prefix, syscall number, or device node. Custom triggers are trivial to add.
- **Reference-counted lifecycle** — `ModuleGuard` prevents automatic unloading while a module is in active use.
- **Safe idle unloading** — a three-phase tick algorithm ensures that blocking I/O (e.g., `unload()`) **never happens while a spinlock is held**.
- **Usage checkers** — optional per-module `UsageChecker` hooks for non-blocking pre-unload cleanup (e.g., unmounting a filesystem before the module exits).
- **Reloadable** — a module that has been unloaded can be triggered and loaded again on the next access.

---

## Architecture

```text
┌─────────────────────────────────────────────────────────────────────┐
│                         ModuleRegistry<L>                            │
│                                                                      │
│  register(desc)  ──►  Registered                                     │
│  on_access(ev)   ──►  Loading  ──success──►  Active                  │
│  acquire(name)   ──►  ModuleGuard  (ref_count++)                     │
│  tick(now)       ──►  Idle  ──timeout──►  Unloading  ──►  Unloaded   │
│                                                                      │
│  Trigger trait          ModuleLoader trait                           │
│  (path / syscall /      (load .ko / unload)                         │
│   device)                                                            │
└─────────────────────────────────────────────────────────────────────┘
```

### State Machine

```text
Registered ──on_access──► Loading ──success──► Active
    ▲                       │                  │    ▲
    │                     fail                 │    │
    │                       ▼         tick()   │  on_access
    │                    Unloaded ◄── Unloading │    │
    │                       ▲           ▲      ▼    │
    │                       │           └── Idle ───┘
    │                   on_access
    │                       │
    └───────────────────────┘ (re-load)
```

| State | Meaning |
|-------|---------|
| `Registered` | Known to the registry, not yet loaded. |
| `Loading` | One thread is currently executing `load()`; concurrent accesses receive `AccessResult::Loading`. |
| `Active` | Loaded and in use (or recently used). |
| `Idle` | Loaded but `ref_count == 0`; timer running. |
| `Unloading` | `prepare_unload()` succeeded; `unload()` is being executed. |
| `Unloaded` | Fully removed; can be re-triggered later. |

### Three-Phase Unload

The idle monitor `tick()` is split into three phases so that potentially blocking `loader.unload()` calls never run inside a critical section:

1. **Phase 1 (under lock)** — scan modules, transition `Active → Idle`, check timeouts, run `prepare_unload()`, and mark candidates as `Unloading`.
2. **Phase 2 (lock released)** — call `loader.unload(handle)` for each candidate. This may perform disk I/O or sleep.
3. **Phase 3 (under lock)** — finalize state. On success → `Unloaded`. On failure → restore `Active` and the handle so a later tick can retry safely.

---

## Quick Start

### 1. Implement `ModuleLoader`

Bridge the framework to your kernel's actual module loader:

```rust
use ondemand_kmod::{ModuleLoader, LoadError, UnloadError};

struct KmodLoader;

impl ModuleLoader for KmodLoader {
    fn load(&self, name: &str, ko_path: &str) -> Result<u64, LoadError> {
        // e.g., read the .ko from disk and call init_module()
        let handle = my_kernel::init_module(ko_path)?;
        Ok(handle)
    }

    fn unload(&self, handle: u64) -> Result<(), UnloadError> {
        my_kernel::delete_module(handle)?;
        Ok(())
    }
}
```

### 2. Create a Registry and Register Modules

```rust
use ondemand_kmod::{ModuleRegistry, ModuleDesc, PathPrefixTrigger, SyscallTrigger};

static REGISTRY: ModuleRegistry<KmodLoader> = ModuleRegistry::new(KmodLoader);

fn init_ondemand() {
    // Load "fuse" when a FUSE mount is accessed
    REGISTRY.register(ModuleDesc {
        name: "fuse",
        ko_path: "/root/modules/fuse.ko",
        idle_timeout_ticks: 5_000, // 5 seconds
        trigger: Box::new(PathPrefixTrigger::new("/mnt/fuse")),
        usage: None,
    });

    // Load "kebpf" when SYS_bpf (321) is invoked
    REGISTRY.register(ModuleDesc {
        name: "kebpf",
        ko_path: "/root/modules/kebpf.ko",
        idle_timeout_ticks: 60_000,
        trigger: Box::new(SyscallTrigger::new(321)),
        usage: None,
    });
}
```

### 3. Hook Into Syscalls / VFS

```rust
use ondemand_kmod::AccessEvent;

fn on_path_access(path: &str) {
    let now = current_tick_ms();
    match REGISTRY.on_access(&AccessEvent::Path(path), now) {
        AccessResult::Loaded => retry_the_vfs_operation(),
        AccessResult::Loading => yield_and_retry(),
        _ => {}
    }
}
```

### 4. Drive the Idle Monitor

Call `tick()` periodically from a timer interrupt or a background kernel task:

```rust
fn timer_callback() {
    REGISTRY.tick(current_tick_ms());
}
```

### 5. (Optional) Implement a `UsageChecker`

If a module owns resources that must be torn down before unloading (e.g., a mounted filesystem), provide a `UsageChecker`:

```rust
use ondemand_kmod::UsageChecker;

struct FuseUsageChecker;

impl UsageChecker for FuseUsageChecker {
    fn is_in_use(&self) -> bool {
        // Must be non-blocking / no I/O
        fuse_driver::has_active_sessions()
    }

    fn prepare_unload(&self) -> Result<(), ()> {
        // Still non-blocking: just signal cleanup
        fuse_driver::start_teardown()
    }
}
```

---

## Integration Example: StarryOS

`ondemand-kmod` was originally developed for StarryOS and has since been extracted as this standalone crate. The StarryOS integration layer (`api/src/kmod/ondemand.rs`) implements `ModuleLoader` by bridging to the existing `kmod-loader` and `ksym` infrastructure.

### Recent Improvements in StarryOS

| Improvement | Summary |
|-------------|---------|
| **Memory profiling infrastructure** | Added `OndemandMemInfo` atomic snapshots (heap + physical pages) around module load/unload boundaries. Exposed via `/proc/ondemand_meminfo` so user-space tests can quantify real memory savings versus the static baseline. |
| **Procfs reverted to static mount** | Procfs was removed from the on-demand registry and restored as a static startup mount. This eliminates repeated unmount/remount noise that previously polluted page-allocator baselines during memory profiling. |
| **FUSE VFS & on-demand integration** | Built out `Starryfuse` (FUSE userspace daemon + kernel VFS bridge) from scratch, fixed ABI opcode mappings, and added `Create`, `Mkdir`, `Setattr`, `Symlink`, `Rename`, and read/write path support, making the FUSE kernel module stable enough for on-demand load/unload stress testing. |
| **Test automation** | Added three user-space test programs (`fuse_test`, `fuse_rw_test`, `fuse_mem_test`) that run inside StarryOS/QEMU to exercise the full on-demand lifecycle and validate memory reclamation after idle timeout. |

### StarryOS Hook Points

In StarryOS, `with_ondemand()` wraps VFS operations so that any syscall returning `NotFound` automatically triggers a module load and retries:

- `resolve_at()` — covers `stat`, `statx`, `access`, `faccessat2`
- `sys_openat()` — covers `open` / `openat`
- `sys_chdir()` / `sys_chroot()` / `sys_readlinkat()` / `sys_statfs()`

This means **one central wrapper** covers all path-based syscalls, rather than requiring fragile per-syscall string pre-matching.

---

## Tests


### Integration tests (StarryOS + QEMU)

The real on-demand loading validation happens inside StarryOS. Build and run the kernel, then execute the following test binaries from the guest shell:

```bash
# 1. Host side: build StarryOS and launch QEMU
make run ARCH=riscv64

# 2. Guest shell: run the three FUSE on-demand test suites
/musl/fuse_test      # basic FUSE mount / lookup / read / write
/musl/fuse_rw_test   # read, write, create, mkdir, setattr, truncate
/musl/fuse_mem_test  # memory profiling: triggers fuse on-demand load,
                     # runs daemon, waits for idle unload,
                     # then reads /proc/ondemand_meminfo and
                     # reports actual memory savings
```

All three binaries are built from `Starryfuse/tests/` and installed into the disk image via `make -C Starryfuse/tests install`.

---

## License

Licensed under either of

- [MIT license](https://opensource.org/licenses/MIT)
- [Apache License, Version 2.0](https://opensource.org/licenses/Apache-2.0)

at your option.
