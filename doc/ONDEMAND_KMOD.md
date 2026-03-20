# StarryOS 按需加载内核模块 (On-Demand Kernel Module Loading)

## 目录

- [项目概述](#项目概述)
- [设计动机](#设计动机)
- [整体架构](#整体架构)
- [状态机](#状态机)
- [新增文件](#新增文件)
  - [ondemand-kmod 独立库](#ondemand-kmod-独立库)
  - [StarryOS 集成层](#starryos-集成层)
- [修改文件](#修改文件)
- [集成流程](#集成流程)
- [如何注册新模块](#如何注册新模块)
- [如何添加新触发器类型](#如何添加新触发器类型)
- [测试说明](#测试说明)
- [编译说明](#编译说明)
- [设计决策与权衡](#设计决策与权衡)
- [已知问题与根因分析](#已知问题与根因分析)

---

## 项目概述

本项目为 StarryOS 实现了**按需加载 / 自动卸载**内核模块的框架。当用户态程序首次访问某个路径（如 `/proc/meminfo`）且 VFS 路径解析返回 `NotFound` 时，系统自动加载对应的内核模块（如 `procfs.ko`）并重试操作。模块空闲超时后自动卸载，释放内存资源。

触发机制采用 **VFS 解析失败重试**模式（而非在每个 syscall 入口做字符串预匹配），通过 `with_ondemand()` 包装 VFS 操作，一个钩子覆盖所有路径类系统调用（open、stat、readlink、access、chdir 等）。

项目由两部分组成：

1. **`ondemand-kmod`** — 独立的 `#![no_std]` Rust 库，提供通用的按需模块管理框架
2. **`api/src/kmod/ondemand.rs`** — StarryOS 内核集成层，桥接框架与现有 LKM 基础设施

## 设计动机

本设计综合了三方面技术：

| 来源 | 技术 | 本项目应用 |
|------|------|-----------|
| AlloyStack | 自动触发加载 (path/syscall/device) | `Trigger` trait 及内建实现 |
| procfs 分支 | 懒挂载注册表 | `ModuleRegistry` 核心数据结构 |
| lkmod 分支 | LKM 加载基础设施 (kmod-loader, ksym) | `KmodOnDemandLoader` 集成调用 |

**目标**：将"什么时候加载"（策略）与"怎么加载"（机制）分离，形成一个可复用的框架。

## 整体架构

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

```text
┌─────────────────────────────────────────────────────────────┐
│                    User Space (应用程序)                      │
│  open("/proc/meminfo")  stat("/proc")  readlink  access ... │
└────────┬───────────────────┬──────────────────┬──────────────┘
         │ sys_openat        │ sys_stat         │ sys_chdir ...
         ▼                   ▼                  ▼
┌─────────────────────────────────────────────────────────────┐
│              api/src/syscall/ + file/fs.rs                    │
│                                                              │
│  with_ondemand(path, || { VFS 操作 })                        │
│    ├─ 先执行 VFS 操作 (resolve / open / ...)                 │
│    ├─ 成功 → 直接返回                                        │
│    └─ NotFound → try_ondemand_load_path(path)                │
│                  ├─ 加载成功 → 重试 VFS 操作                  │
│                  └─ 无匹配 → 返回原始 NotFound                │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│            api/src/kmod/ondemand.rs  (集成层)                 │
│                                                              │
│  REGISTRY: Once<ModuleRegistry<KmodOnDemandLoader>>          │
│                                                              │
│  ┌────────────────────────────────────────────────────┐      │
│  │  KmodOnDemandLoader (impl ModuleLoader)            │      │
│  │    load()  → read_ko_file() → init_module()       │      │
│  │    unload()→ MODULES.find()  → delete_module()    │      │
│  └────────────────────────────────────────────────────┘      │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│           ondemand-kmod/ (独立 no_std 库)                     │
│                                                              │
│  registry.rs   ← ModuleRegistry<L>  核心注册表               │
│  lifecycle.rs  ← State 状态机 + ModuleGuard 引用计数          │
│  trigger.rs    ← Trigger trait + PathPrefix/Syscall/Device   │
│  monitor.rs    ← IdleMonitor 空闲扫描及自动卸载               │
│  loader.rs     ← ModuleLoader / UsageChecker trait           │
└─────────────────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│         kmod-loader / ksym / kapi  (现有 LKM 基础设施)        │
│  ELF 解析 → 重定位 → 符号绑定 → init/exit 调用               │
└─────────────────────────────────────────────────────────────┘
```

**定时器驱动**的自动卸载路径：

```text
Timer IRQ → api/src/lib.rs timer callback
         → kmod::ondemand::tick_ondemand()
         → registry.tick(current_tick())
         → IdleMonitor::tick()  (三阶段扫描)
```

## 状态机

每个受管理模块的生命周期状态：

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
    └───────────────────────┘ (重新加载)
```

| 状态 | 含义 | 转换条件 |
|------|------|---------|
| `Registered` | 已注册，尚未加载 | 初始状态 |
| `Loading` | 正在加载（某线程持有） | `on_access` 命中触发器 |
| `Active` | 已加载，可能有活跃引用 | `load()` 成功 |
| `Idle` | 已加载，引用计数为 0 | `tick()` 检测到无引用 |
| `Unloading` | 正在卸载 | 空闲超时且 `prepare_unload()` 成功 |
| `Unloaded` | 已卸载，等待重新加载 | `unload()` 完成 |

关键特性：
- **并发安全**：`Loading` 状态防止多线程重复加载
- **可重入**：`Unloaded` 模块可通过 `on_access` 重新触发加载
- **无锁加载/卸载**：实际 I/O 操作在释放自旋锁后执行

## 新增文件

### ondemand-kmod 独立库

#### `ondemand-kmod/Cargo.toml`

```toml
[package]
name = "ondemand-kmod"
version = "0.1.0"
edition = "2021"
description = "On-demand kernel module loading and unloading framework"

[dependencies]
spin = { version = "0.9", default-features = false, features = ["mutex", "spin_mutex"] }
```

唯一外部依赖为 `spin` 自旋锁（`no_std` 环境下替代 `std::sync::Mutex`）。

---

#### `ondemand-kmod/src/lib.rs` — 入口

- 声明 `#![no_std]` 和 `extern crate alloc`
- 将五个内部模块的公有类型重导出

```rust
pub use lifecycle::{AccessResult, ModuleDesc, ModuleGuard, ModuleInfo, State};
pub use loader::{LoadError, ModuleLoader, UnloadError, UsageChecker};
pub use registry::ModuleRegistry;
pub use trigger::{AccessEvent, DeviceTrigger, PathPrefixTrigger, SyscallTrigger, Trigger};
```

---

#### `ondemand-kmod/src/loader.rs` — 加载器 trait

定义两个核心 trait，由内核集成层实现：

| Trait | 方法 | 说明 |
|-------|------|------|
| `ModuleLoader` | `load(name, ko_path) → Result<u64, LoadError>` | 加载 `.ko` 文件并执行 init |
| `ModuleLoader` | `unload(handle) → Result<(), UnloadError>` | 卸载模块并执行 exit |
| `UsageChecker` | `is_in_use() → bool` | 检查模块是否有活跃用户 |
| `UsageChecker` | `prepare_unload() → Result<(), ()>` | 预卸载清理（必须非阻塞） |

错误类型：
- `LoadError`: `NotFound`, `InvalidModule`, `InitFailed(i32)`, `Other`
- `UnloadError`: `InUse`, `NotLoaded`, `ExitFailed`, `Other`

---

#### `ondemand-kmod/src/trigger.rs` — 触发器

`AccessEvent<'a>` 枚举表示三种访问事件：

```rust
pub enum AccessEvent<'a> {
    Path(&'a str),      // 文件路径访问
    Syscall(usize),     // 系统调用号
    Device(&'a str),    // 设备节点访问
}
```

`Trigger` trait 及三个内建实现：

| 实现 | 匹配规则 | 示例 |
|------|---------|------|
| `PathPrefixTrigger` | 路径前缀匹配（含边界检查） | `"/proc"` → 匹配 `/proc/meminfo`，不匹配 `/process` |
| `SyscallTrigger` | 精确系统调用号匹配 | `SyscallTrigger::new(321)` 匹配 SYS_bpf |
| `DeviceTrigger` | 设备路径前缀匹配 | `"/dev/null_blk"` → 匹配 `/dev/null_blk0` |

路径边界感知：前缀后必须是 `/` 或字符串结尾，防止 `/proc` 误匹配 `/process`。

---

#### `ondemand-kmod/src/lifecycle.rs` — 生命周期

核心类型：

- **`State`** — 6 状态枚举（见上文状态机）
- **`ModuleDesc`** — 模块描述符
  - `name: &'static str` — 唯一模块名
  - `ko_path: &'static str` — `.ko` 文件路径
  - `idle_timeout_ticks: u64` — 空闲超时（0 = 禁用自动卸载）
  - `trigger: Box<dyn Trigger>` — 触发器
  - `usage: Option<Box<dyn UsageChecker>>` — 可选使用检查器
- **`ModuleGuard`** — RAII 引用守卫
  - 持有 `Arc<AtomicUsize>` 引用计数
  - Drop 时自动减计数
  - 存在期间阻止自动卸载
- **`AccessResult`** — `on_access` 返回值（`NoMatch`, `Loaded`, `Loading`, `LoadFailed`, `Unavailable`）
- **`ModuleInfo`** — 模块状态快照（只读）
- **`ManagedModule`** (pub(crate)) — 内部运行时记录

---

#### `ondemand-kmod/src/monitor.rs` — 空闲监视器

`IdleMonitor::tick()` 实现三阶段扫描算法：

```text
Phase 1 (持锁):
  ① Active → Idle: ref_count == 0 时转为空闲
  ② 检查 Idle 模块是否超时
  ③ 调用 prepare_unload() (必须非阻塞)
  ④ 标记为 Unloading, 收集待卸载列表

Phase 2 (无锁):
  ⑤ 调用 loader.unload(handle) (可能涉及 I/O)

Phase 3 (持锁):
  ⑥ 最终状态转为 Unloaded
```

将 I/O 操作移出锁范围，避免自旋锁持有期间的阻塞。

---

#### `ondemand-kmod/src/registry.rs` — 模块注册表

`ModuleRegistry<L: ModuleLoader>` 是核心数据结构：

| 方法 | 签名 | 说明 |
|------|------|------|
| `new` | `const fn new(loader: L) -> Self` | 创建空注册表 |
| `register` | `fn register(&self, desc: ModuleDesc) -> bool` | 注册模块 |
| `on_access` | `fn on_access(&self, event: &AccessEvent, now: u64) -> AccessResult` | 触发加载 |
| `acquire` | `fn acquire(&self, name: &str, now: u64) -> Option<ModuleGuard>` | 获取引用 |
| `tick` | `fn tick(&self, now: u64)` | 空闲扫描 |
| `force_unload` | `fn force_unload(&self, name: &str) -> Result<(), UnloadError>` | 强制卸载 |
| `state_of` | `fn state_of(&self, name: &str) -> Option<State>` | 查询状态 |
| `list_modules` | `fn list_modules(&self) -> Vec<ModuleInfo>` | 列出所有模块 |

`on_access` 采用**无锁加载模式**：先持锁确认需要加载并设为 `Loading`，释放锁后执行 `loader.load()`，再持锁更新状态。

---

### StarryOS 集成层

#### `api/src/kmod/ondemand.rs` — 集成桥梁

**`KmodOnDemandLoader`** — 实现 `ModuleLoader` trait：

```rust
impl ModuleLoader for KmodOnDemandLoader {
    fn load(&self, name: &str, ko_path: &str) -> Result<u64, LoadError> {
        let elf_data = read_ko_file(ko_path)?;  // 从文件系统读取 .ko
        super::init_module(&elf_data, None)?;     // 调用现有 LKM 加载
        Ok(simple_hash(name))                     // 返回名称哈希作为 handle
    }

    fn unload(&self, handle: u64) -> Result<(), UnloadError> {
        // 通过 handle (名称哈希) 在 MODULES 全局表中查找
        let name = modules.keys().find(|k| simple_hash(k) == handle);
        super::delete_module(&name)?;             // 调用现有 LKM 卸载
    }
}
```

**辅助函数**：

| 函数 | 可见性 | 说明 |
|------|--------|------|
| `with_ondemand(path, f)` | `pub` | **核心钩子**：先执行 `f()`，若返回 `NotFound` 则尝试加载模块并重试 |
| `try_ondemand_load_path(path)` | `fn` (私有) | 路径触发内部实现，含重试逻辑 (5 次，Loading/Unavailable 时 yield) |
| `try_ondemand_load_syscall(sysno)` | `pub` | 系统调用触发入口 |
| `init_ondemand()` | `pub` | 初始化全局 `REGISTRY` (Once) |
| `registry()` | `pub` | 获取全局注册表引用 |
| `tick_ondemand()` | `pub` | 定时器回调入口 |
| `read_ko_file(path)` | `fn` | 通过 `axfs_ng` 读取 `.ko` 文件 (4096 字节缓冲) |
| `simple_hash(s)` | `fn` | djb2 哈希，将模块名映射为 `u64` handle |
| `current_tick()` | `fn` | 获取当前毫秒级时间戳 |

#### `modules/procfs/Cargo.toml` 与 `modules/procfs/src/lib.rs`

新增独立的 `procfs.ko` 模块 crate，用于承载 procfs 的挂载/卸载逻辑：

- `procfs_init()`：直接使用 `axfs_ng::FS_CONTEXT` 创建 `/proc` 挂载点并挂载 `starry_api::vfs::new_procfs()`
- `procfs_exit()`：直接解析 `/proc` 并执行 `unmount()`
- 模块侧自行依赖 `axfs-ng` 与 `axfs-ng-vfs`，避免在原有 `api/src/vfs/mod.rs` 中增加专用挂载封装
- 通过 `module!` 宏导出模块元数据（`name = "procfs"`）

该设计使 procfs 由静态内核功能转为真正的按需可加载模块。

---

## 修改文件

### 1. `Cargo.toml`（工作空间根）

```diff
-members = ["api", "core", "user/musl/async_test", "modules/hello", "modules/kebpf"]
+members = ["api", "core", "ondemand-kmod", "modules/procfs", "user/musl/async_test", "modules/hello", "modules/kebpf"]
```

```diff
 kmod = { git = "https://github.com/Starry-OS/rkm" }
+ondemand-kmod = { path = "./ondemand-kmod" }
```

```diff
-axplat-aarch64-qemu-virt = { git = "https://github.com/Starry-OS/axplat_crates.git", rev = "243fdc9" }
+# axplat-aarch64-qemu-virt: crates.io fallback (Starry-OS/arm-gic-driver repo deleted)
```

> **说明**：`arm-gic-driver` 仓库已从 GitHub 删除，注释掉 `axplat-aarch64-qemu-virt` 的 git patch 使其回退到 crates.io 版本。

### 2. `api/Cargo.toml`

```diff
 kapi.workspace = true
+ondemand-kmod.workspace = true
```

### 3. `api/src/kmod/mod.rs`

```diff
 mod shim;
+pub mod ondemand;
```

```diff
 pub fn init_kmod() {
     lwprintf_rs::lwprintf_init::<StdOut>();
+    ondemand::init_ondemand();
     ax_println!("kmod subsystem initialized");
 }
```

### 4. `api/src/lib.rs`

在定时器回调中添加 tick 调用：

```diff
 axtask::register_timer_callback(|_| {
     time::inc_irq_cnt();
+    kmod::ondemand::tick_ondemand();
 });
```

### 5. `api/src/file/fs.rs` — `resolve_at()`

用 `with_ondemand` 包裹路径解析，覆盖所有通过 `resolve_at` 的 syscall（stat、statx、access、faccessat2 等）：

```diff
-Some(path) => with_fs(dirfd, |fs| {
-    if flags & AT_SYMLINK_NOFOLLOW != 0 {
-        fs.resolve_no_follow(path)
-    } else {
-        fs.resolve(path)
-    }
-    .map(ResolveAtResult::File)
-}),
+Some(path) => {
+    let do_resolve = |fs: &mut FsContext| { ... };
+    crate::kmod::ondemand::with_ondemand(path, || with_fs(dirfd, &do_resolve))
+}
```

### 6. `api/src/syscall/fs/fd_ops.rs` — `sys_openat`

用 `with_ondemand` 包裹 open 操作（替代旧的预匹配 hook）：

```diff
-crate::kmod::ondemand::try_ondemand_load_path(&path);
-with_fs(dirfd, |fs| options.open(fs, path))
+crate::kmod::ondemand::with_ondemand(&path, || {
+    with_fs(dirfd, |fs| options.open(fs, &path))
+})
```

### 7. `api/src/syscall/fs/ctl.rs` — `sys_chdir` / `sys_chroot` / `sys_readlinkat`

用 `with_ondemand` 包裹 resolve 操作（替代旧的预匹配 hook）：

```diff
-crate::kmod::ondemand::try_ondemand_load_path(&path);
-let mut fs = FS_CONTEXT.lock();
-let entry = fs.resolve(path)?;
+let entry = crate::kmod::ondemand::with_ondemand(&path, || {
+    Ok(FS_CONTEXT.lock().resolve(&path)?)
+})?;
```

同理 `sys_chroot`、`sys_readlinkat`、`sys_statfs` 也做了相同包裹。

### 8. `api/src/syscall/fs/stat.rs` 

```diff
-buf.vm_write(statfs(
-    &FS_CONTEXT
-        .lock()
-        .resolve(path)?
-        .mountpoint()
-        .root_location(),
-)?)?;
-    .map(|loc| loc.mountpoint().root_location())?;
-buf.vm_write(statfs(&loc)?)?;
+let loc = crate::kmod::ondemand::with_ondemand(&path, || {
+    Ok(FS_CONTEXT.lock().resolve(&path)?)
+})?;
+buf.vm_write(statfs(&loc.mountpoint().root_location())?)?;
```

### 9. `api/src/vfs/mod.rs` 与 `api/src/vfs/proc.rs`

将 procfs 从启动期静态挂载改为模块生命周期挂载：

```diff
-mount_at("/proc", proc::new_procfs(ksym))
+proc::init_kallsyms(ksym)
```

同时将 procfs 的挂载/卸载逻辑保留在模块侧，`api/src/vfs/mod.rs` 只额外导出 procfs 工厂函数。

```rust
pub use proc::new_procfs;
```

同时在 `api/src/vfs/proc.rs` 中拆分初始化逻辑：
- `init_kallsyms(kallsyms)`：仅初始化 `GLOBAL_KALLSYMS`
- `new_procfs()`：构造 procfs 实例，不再接收 kallsyms 参数

### 10. `api/src/kmod/ondemand.rs` 与 `api/src/kmod/ondemand_builtin.rs`

将按需加载框架桥接与模块策略注册解耦：

- `ondemand.rs`：仅负责 `ModuleLoader` 适配、全局 `REGISTRY`、`with_ondemand()` 与 `tick_ondemand()`，保持泛型
- `ondemand_builtin.rs`：承载 StarryOS 内建策略（如 procfs 的路径触发注册）

在 `ondemand_builtin::register_builtin_modules()` 中注册 procfs：

```rust
let _ = super::ondemand::register_module(ModuleDesc {
    name: "procfs",
    ko_path: "/root/modules/procfs.ko",
    idle_timeout_ticks: 5_000,
    trigger: Box::new(PathPrefixTrigger::new("/proc")),
    usage: None,
});
```

### 11. `ondemand-kmod/src/monitor.rs`

修复自动卸载状态机的一致性问题：

- 旧行为：`loader.unload(handle)` 失败时，Phase 3 仍会把模块状态置为 `Unloaded`
- 新行为：仅当 unload 成功才置 `Unloaded`
- unload 失败时：恢复 `State::Active`、恢复 `loaded_handle` 并 `touch(now)`，后续 tick 可安全重试

该修复避免了“卸载失败但状态被错误推进”导致的句柄丢失与后续不一致。

### 12. `api/src/kmod/ondemand.rs`

为 `procfs` 增加两阶段卸载保护（loader 层）：

- 第一次触发 unload：先尝试 `unmount /proc`，并返回 `UnloadError::InUse`（延后真正模块释放）
- 后续 tick 再次调用 unload：若 `/proc` 已不是挂载根，再执行 `delete_module("procfs")`

该策略避免“刚卸载挂载点就立刻释放模块内存”导致的悬挂引用窗口。

### 13. `modules/procfs/src/lib.rs`

将 `procfs_exit()` 改为轻量收尾日志，不再重复执行 `unmount()`。

原因：`/proc` 卸载已在上层两阶段卸载路径中完成，模块 `exit_fn` 只负责退出收尾，避免重复卸载副作用。

---

## 集成流程

### 启动流程

```text
1. 内核启动 → api/src/lib.rs::init()
2.   → kmod::init_kmod()
3.     → lwprintf_init()
4.     → ondemand::init_ondemand()   ← 初始化全局 REGISTRY
5.     → ondemand_builtin::register_builtin_modules()  ← 注册内建模块策略
6.   → register_timer_callback(tick_ondemand)  ← 注册定时 tick
```

### 运行时触发流程

```text
用户程序: open("/proc/meminfo", O_RDONLY)  或 stat/readlink/access/chdir...
  │
  ▼
sys_openat() / sys_fstatat() / sys_readlinkat() / ...
  │
  ▼
with_ondemand("/proc/meminfo", || { VFS open/resolve })
  │
  ├─ 第 1 次尝试: VFS open("/proc/meminfo")
  │   └─ /proc 未挂载 → 返回 NotFound
  │
  ├─ 检测到 NotFound → try_ondemand_load_path("/proc/meminfo")
  │   ├─ registry.on_access(Path("/proc/meminfo"), now)
  │   │   ├─ PathPrefixTrigger("/proc").matches() → true
  │   │   ├─ State: Registered → Loading (持锁)
  │   │   ├─ (释放锁)
  │   │   ├─ loader.load("procfs", "/root/modules/procfs.ko")
  │   │   │   ├─ read_ko_file() → Vec<u8>
    │   │   │   └─ init_module(elf_data) → 调用 procfs_init() → 直接挂载 /proc
  │   │   ├─ (持锁) State: Loading → Active
  │   │   └─ return AccessResult::Loaded
  │   └─ return true (加载成功)
  │
  ├─ 第 2 次尝试: VFS open("/proc/meminfo")
  │   └─ /proc 已挂载 → 成功返回 File
  │
  └─ 返回打开的文件
```

与旧方案的对比：

| 旧方案（syscall 预匹配） | 新方案（VFS 失败重试） |
|--------------------------|----------------------|
| 在每个 syscall 入口插入 `try_ondemand_load_path` | 在 VFS 操作处用 `with_ondemand` 包裹 |
| 需逐个 hook：openat, chdir, ... | 一个 `resolve_at` 覆盖 stat/statx/access 等所有路径 syscall |
| 无论路径是否存在都触发检测 | 只在 `NotFound` 时才触发，正常路径零开销 |
| 容易遗漏 syscall | 所有路径操作汇聚到 resolve，不会遗漏 |

### 自动卸载流程

```text
Timer IRQ (每次中断)
  │
  ▼
tick_ondemand()
  └─ registry.tick(current_tick_ms)
        └─ IdleMonitor::tick()
          ├─ Phase 1 (持锁):
          │   ├─ procfs: Active, ref_count=0 → Idle, idle_since=now
          │   ├─ (下一次 tick) procfs: Idle, 超时? yes
          │   ├─ prepare_unload() → Ok
          │   └─ State: Idle → Unloading, 记录 handle
          ├─ Phase 2 (无锁):
          │   ├─ 第一次: loader.unload(handle)
          │   │   ├─ 先 unmount /proc
          │   │   └─ 返回 InUse（延后释放模块内存）
          │   └─ 后续 tick: 再次 loader.unload(handle) → delete_module("procfs")
          └─ Phase 3 (持锁):
            ├─ unload 成功: State → Unloaded
            └─ unload 失败: State → Active，并恢复 handle（可重试）
```

---

## 如何注册新模块

推荐在独立策略文件（如 `api/src/kmod/ondemand_builtin.rs`）中注册，避免污染框架桥接层：

```rust
use alloc::boxed::Box;
use ondemand_kmod::{ModuleDesc, PathPrefixTrigger, SyscallTrigger};

pub fn register_builtin_modules() {
    // 示例：当访问 /proc 路径时自动加载 procfs 模块
    let _ = super::ondemand::register_module(ModuleDesc {
            name: "procfs",
            ko_path: "/root/modules/procfs.ko",
            idle_timeout_ticks: 30_000, // 30 秒空闲后自动卸载
            trigger: Box::new(PathPrefixTrigger::new("/proc")),
            usage: None,
    });

    // 示例：当调用 SYS_bpf (321) 时加载 eBPF 模块
    let _ = super::ondemand::register_module(ModuleDesc {
            name: "kebpf",
            ko_path: "/root/modules/kebpf.ko",
            idle_timeout_ticks: 60_000, // 60 秒
            trigger: Box::new(SyscallTrigger::new(321)),
            usage: None,
    });
}
```

对于需要自定义使用检查的模块，实现 `UsageChecker` trait：

```rust
struct ProcfsUsageChecker;

impl UsageChecker for ProcfsUsageChecker {
    fn is_in_use(&self) -> bool {
        // 检查是否有打开的 /proc 文件描述符
        false
    }
    fn prepare_unload(&self) -> Result<(), ()> {
        // 卸载前清理：umount /proc
        Ok(())
    }
}

// 注册时传入
reg.register(ModuleDesc {
    // ...
    usage: Some(Box::new(ProcfsUsageChecker)),
});
```

## 如何添加新触发器类型

实现 `Trigger` trait：

```rust
use ondemand_kmod::{Trigger, AccessEvent};

/// 当路径包含指定子串时触发
struct PathContainsTrigger {
    substring: &'static str,
}

impl Trigger for PathContainsTrigger {
    fn matches(&self, event: &AccessEvent) -> bool {
        match event {
            AccessEvent::Path(path) => path.contains(self.substring),
            _ => false,
        }
    }
}
```

然后在 `ModuleDesc` 中使用：

```rust
reg.register(ModuleDesc {
    trigger: Box::new(PathContainsTrigger { substring: "my_feature" }),
    // ...
});
```

## 测试说明

### 1. `ondemand-kmod` 单元测试

```bash
cargo test -p ondemand-kmod
```

当前包含两类关键用例：
- `load_on_access_and_unload_after_idle_timeout`：验证访问触发加载与空闲超时卸载
- `no_match_does_not_load_module`：验证非匹配路径不会误触发加载

### 2. procfs 按需加载集成测试（QEMU）

脚本位置：`scripts/test-ondemand-procfs.py`

```bash
python3 scripts/test-ondemand-procfs.py \
    --kernel path/to/kernel.elf \
    --disk path/to/disk.img
```

脚本验证流程：
- 启动 QEMU 并等待 shell 就绪
- 访问 `/proc` 路径触发 `procfs.ko` 加载
- 检查日志中是否出现加载成功信息
- 等待空闲超时并检查是否出现自动卸载信息

### 3. 语法与构建检查

```bash
python3 -m py_compile scripts/test-ondemand-procfs.py
cargo check -p procfs --target riscv64gc-unknown-none-elf --features qemu
```

## 编译说明

### 环境要求

- Rust nightly（工具链版本见 `rust-toolchain.toml`）
- RISC-V 64 交叉编译工具链：`riscv64-linux-musl-cross`
- 目标三元组：`riscv64gc-unknown-none-elf`

### 编译检查

```bash
# 在 lkmod 分支上
git checkout lkmod

# 编译检查（不生成最终二进制）
cargo check --target riscv64gc-unknown-none-elf --features qemu
```

### 完整构建

```bash
# 完整构建（需要 QEMU 环境）
make A=. ARCH=riscv64 qemu
```

### 已验证

```
cargo check --target riscv64gc-unknown-none-elf --features qemu
# 结果: 0 errors, 0 warnings
```

## 设计决策与权衡

### 1. 独立库 vs 内联实现

选择将按需加载逻辑实现为独立 `ondemand-kmod` crate，而非直接写入 `api/` 中：
- **可测试性**：`no_std` 库可在宿主机上用 mock loader 进行单元测试
- **可复用性**：不依赖 StarryOS 特定类型，可移植到其他 unikernel
- **关注点分离**：策略（触发器、超时）与机制（ELF 加载）解耦

### 2. 自旋锁 vs 睡眠锁

选择 `spin::Mutex`：
- 内核环境无法使用 `std::sync::Mutex`
- 锁持有时间极短（仅状态转换），不会有长时间自旋
- 实际 I/O（加载/卸载）在锁外执行

### 3. 三阶段 tick 算法

将 `IdleMonitor::tick()` 分为三阶段（持锁→无锁→持锁）：
- 避免在自旋锁内执行可能阻塞的文件 I/O
- 代价是中间阶段其他线程可对模块状态做出改变，需在 Phase 3 重新验证

### 4. djb2 哈希作为模块 handle

`ModuleLoader::unload(handle)` 接口只传 `u64` handle：
- 选择 djb2 哈希将模块名映射为 handle
- 卸载时遍历 `MODULES` 全局表找到对应模块名
- 简单够用，碰撞概率极低（模块数量有限）

### 5. 定时器回调中的 tick

在每次 timer IRQ 中调用 `tick_ondemand()`：
- 如果没有空闲模块，`tick()` 几乎是 no-op（只遍历一次 Vec）
- 频率取决于定时器中断频率（通常 100-1000 Hz）
- `idle_timeout_ticks` 以毫秒为单位，可灵活配置超时时间

### 6. VFS 失败重试 vs syscall 预匹配

最初采用在各 syscall 入口（`sys_openat`、`sys_chdir`）插入 `try_ondemand_load_path(&path)` 的方案，存在以下问题：
- **覆盖不全**：遗漏了 `stat`、`readlink`、`access`、`readdir` 等同样走路径的 syscall
- **位置不对**：在"调用者"检测而非"被调用者"处，属于间接推断
- **无条件触发**：路径已存在时仍执行触发检测，浪费开销

改为 **VFS 失败重试**方案：
```rust
pub fn with_ondemand<R>(path: &str, f: impl Fn() -> AxResult<R>) -> AxResult<R> {
    match f() {
        Err(AxError::NotFound) if try_ondemand_load_path(path) => f(),
        other => other,
    }
}
```
在 `resolve_at()`、`sys_openat`、`sys_chdir`、`sys_chroot`、`sys_readlinkat`、`sys_statfs` 处包裹 VFS 操作。只在真正 `NotFound` 时才触发模块加载。

### 7. arm-gic-driver 仓库问题

`Starry-OS/arm-gic-driver` GitHub 仓库已被删除，导致 `[patch.crates-io]` 中的 `axplat-aarch64-qemu-virt` 无法解析。解决方案：
- 注释掉该 patch 行，使 aarch64 平台回退到 crates.io 版本
- 仅影响 aarch64 目标，不影响 RISC-V 64 编译

## 已知问题与根因分析

> 状态日期：2026-03-14

### 现象

在 QEMU 中执行以下序列时会出现崩溃：

1. `cat /proc/meminfo` 触发 `procfs.ko` 按需加载并读取成功
2. 空闲超时后触发自动卸载（日志显示 unload + exit + 内存释放）
3. 随后执行普通命令（如 `ls`）触发内核异常（Page Fault / IllegalInstruction）

### 已确认成功部分

- 按需加载链路成功：`NotFound` → `with_ondemand()` → `load(procfs.ko)` → `procfs_init()` → `/proc` 可读
- 自动卸载链路可触发：定时器 tick 会进入 unload 路径并调用模块退出

### 根因判断（当前结论）

当前问题不在“是否触发按需加载”，而在“文件系统卸载的生命周期安全性”：

1. `axfs-ng-vfs` 的 `Location::unmount()` 主要完成命名空间摘除（断开挂载点），但未提供“活跃引用归零”保障
2. `axfs-ng` 文件/缓存对象会长期持有 `Location`，卸载后仍可能存在残留引用
3. 当模块内存被释放后，后续普通 VFS 路径访问可能命中失效对象，导致 trap
4. 自动卸载目前由 timer 回调驱动，执行上下文为 irq/preemption-disabled，进一步放大了卸载路径风险

### 目前补丁状态

已实现的缓解项：

- `ondemand-kmod`：修复 unload 失败时状态机误推进（失败不再强制置 `Unloaded`）
- `starry-api`：为 procfs 增加“两阶段卸载”尝试（先 unmount，后续 tick 再 free）

结论：上述缓解项可降低时序风险，但不能从根本上保证卸载安全，崩溃仍可能出现。

### 正式修复方向

正式方案需在 VFS 层建立“可验证的安全卸载语义”，至少包括：

1. 为挂载点引入活跃引用/占用判定（或等效 busy 机制）
2. `unmount` 仅在无活跃引用时成功，否则返回 busy
3. 将真正的卸载执行从 timer IRQ 路径迁移到任务上下文
4. `ondemand` 层仅负责调度卸载请求，不在 IRQ 上下文直接执行文件系统卸载与模块释放

在上述基础设施完成前，`procfs` 自动卸载不应视为稳定功能。
