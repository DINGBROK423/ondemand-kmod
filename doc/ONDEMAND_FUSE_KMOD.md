# StarryOS 用户态文件系统 (FUSE) 按需加载实现详情

## 目录

- [项目概述](#项目概述)
- [设计动机](#设计动机)
- [整体架构](#整体架构)
- [新增与核心文件结构](#新增与核心文件结构)
  - [Starryfuse 独立库](#starryfuse-独立库)
  - [fuse 可加载模块 (LKM)](#fuse-可加载模块-lkm)
- [核心集成与修改说明](#核心集成与修改说明)
  - [按需加载注册与触发机制](#按需加载注册与触发机制)
  - [空闲卸载与使用检查器 (Usage Checker)](#空闲卸载与使用检查器-usage-checker)
- [集成与执行流程](#集成与执行流程)
- [设计决策与权衡：为何 FUSE 的按需加载更安全](#设计决策与权衡为何-fuse-的按需加载更安全)
- [已知问题与根因分析 (非常重要)](#已知问题与根因分析-非常重要)

---

## 项目概述

本项目为 StarryOS 引入了对 **FUSE (Filesystem in Userspace)** 的支持，并将其深度整合到了系统的 **按需加载内核模块 (On-Demand LKM)** 框架中。

核心理念是：系统启动时，内核中不包含任何与 FUSE 相关的常驻驱动代码。只有当用户态程序（如 `fuse_test`）首次试图访问 `/dev/fuse` 字符设备时，VFS 捕获到 NotFound，系统才会动态将 `fuse.ko` 装载至内核，创建 FUSE 字符设备节点并开启 IPC（进程间通信）。当所有 FUSE 文件系统挂载点被卸载，且 `/dev/fuse` 句柄被彻底关闭超时后，内核将自动彻底卸载 `fuse.ko` 释放内存。

## 设计动机

将 FUSE 剥离为主内核以外的独立模块体系，主要基于微内核和全解耦的设计哲学：
1. **启动时轻量**：用户态文件系统属于高位功能，默认环境大部分程序不需要挂载 FUSE，内核核心不应为此承担内存和增加体积。
2. **状态与异常隔离**：用户态文件系统自身逻辑可能出错，将其通信总线（设备节点和队列）做成模块，能够随时随地装卸，而不会导致主 VFS 僵死。
3. **符合标准 `ondemand` 范式**：FUSE 需要通过标准的按需加载流程验证 StarryOS 动态模块基础设施的稳健性。

## 整体架构

```text
┌─────────────────────────────────────────────────────────────┐
│                    User Space (应用程序)                      │
│   fuse_test: open("/dev/fuse")   /   mount("/mnt/fuse")     │
└────────┬────────────────────────────────────────────────────┘
         │ 触发
         ▼
┌─────────────────────────────────────────────────────────────┐
│               StarryOS VFS (api/src/syscall/)                 │
│                                                             │
│   with_ondemand("/dev/fuse", || { VFS resolve })            │
│    └─ 解析失败 → 触发 try_ondemand_load_path()               │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│                ondemand-kmod 框架 & 模块加载                  │
│                                                             │
│  加载 /root/modules/fuse.ko ────────► 映射入内核内存          │
└────────────────────────┬────────────────────────────────────┘
                         │ 
                         ▼
┌─────────────────────────────────────────────────────────────┐
│               模块初始化 (modules/fuse/)                      │
│                                                             │
│   fuse_init() ───►  starryfuse::init_fuse()                 │
│                            │                                │
│                   (注：此处不碰复杂的 FS_CONTEXT)              │
│                            │                                │
│                            ▼                                │
│       API 调用: register_devfs_device("fuse", 10, 229)      │
│        └► 在内存文件系统 devfs 挂载点创建 /dev/fuse 节点         │
└─────────────────────────────────────────────────────────────┘
```

## 新增与核心文件结构

FUSE 的支持被划分为“功能核心”和“模块包装层”两个级别的 Crate：

### Starryfuse 独立库
**位置**：`/workspaces/StarryOS/Starryfuse/*`
负责真正的 FUSE 通信协议和数据运转逻辑。
- `src/lib.rs`：初始化入口，维护全局静态变量 `FUSE_CONNECTION`，以及负责调用底层注册 API `register_devfs_device("fuse", ...)` 创建虚拟字符节点，并规避了主文件系统的上下文锁定。
- `src/dev.rs`：定义 `FuseDev` 结构并实现 `DeviceOps`，处理诸如 `read`, `write`, `poll` 的行为，使能用户态 Daemon 和内核之间的请求出入队通信。
- `tests/fuse_test/src/main.rs`：标准的用户态 FUSE Demon。验证 `open("/dev/fuse")`、构建挂载、监听返回 Request 和调度 `handle_lookup` / `handle_getattr` 的正确性。

### fuse 可加载模块 (LKM)
**位置**：`/workspaces/StarryOS/modules/fuse/*`
将 `Starryfuse` 的静态能力包裹为 `.ko`。
- `Cargo.toml`：依赖 `starryfuse`、`axfeat`、`kmod` 等。
- `src/lib.rs`：暴露出 `#[init_fn]` 的 `fuse_init` 和 `#[exit_fn]`的 `fuse_exit`。在模块加载和卸载时分别调用底层的相关处理。

## 核心集成与修改说明

### 按需加载注册与触发机制
**文件**：`api/src/kmod/ondemand_builtin.rs`
由 `register_builtin_modules()` 负责注册：
```rust
let _ = super::ondemand::register_module(ModuleDesc {
    name: "fuse",
    ko_path: "/root/modules/fuse.ko",
    idle_timeout_ticks: 5_000,
    trigger: Box::new(PathPrefixTrigger::new("/dev/fuse")),
    usage: Some(Box::new(FuseUsageChecker)),
});
```
只要用户触碰 `/dev/fuse`，就会触发装载。装载函数无需强制重置 `FS_CONTEXT` 即可将资源暴露进 `devfs`。

### 空闲卸载与使用检查器 (Usage Checker)
为了保证内核模块在真正空闲时才被卸载，同时避免轮询导致系统灾难，框架专门实现了精巧的 `FuseUsageChecker`。
```rust
impl UsageChecker for FuseUsageChecker {
    fn is_in_use(&self) -> bool {
        // ...
        // 遍历整个 OS 的 FD Table
        if let Some(file) = any.downcast_ref::<crate::file::File>() {
            let loc = file.inner().location();
            let fs_name = loc.filesystem().name();
            // 规则 1：系统存在任何直接被 FUSE 文件系统挂载的文件描述符
            if fs_name == "fuse" { return true; } 
            
            // 规则 2：仅仅当它身处 devfs，我们才安全地获取完整文本 path
            else if fs_name == "devfs" {
                let fpath = crate::file::FileLike::path(file);
                if fpath == "/dev/fuse" || fpath == "/fuse" || fpath == "fuse" {
                    return true;
                }
            }
        }
        // ...
    }
}
```

## 集成与执行流程

1. 用户进程（如 `fuse_test`）发起 `open("/dev/fuse", O_RDWR)`。
2. StarryOS VFS `sys_openat` 层向下透传，发现找不到路径。触发 `with_ondemand` hook。
3. 加载器挂载 `fuse.ko`。
4. 模块初始化仅调用 `register_devfs_device`，在 `devfs` 的内存映射树种插入 FUSE Node。
5. 原本的 VFS 请求重新执行，此时成功抓到该节点，并将其作为文件描述符 (fd) 发给该用户程序。
6. Timer 中断每隔 tick() 唤醒 `ondemand` 后台监视器。监视器调用 `FuseUsageChecker` 轮询 VFS 与 FD，确认是否有活跃 FUSE 引用，以此管理空闲回收倒计时。

## 设计决策与权衡：为何 FUSE 的按需加载更安全

在实现文件系统级的按需加载中，FUSE 采用的设计方案（向纯内存的 `devfs` 挂载 `register_devfs_device`）展现了极佳的容错性，对比 `procfs` 等其它模块实现：
* FUSE 的模块不会去访问基于宏展开和当前作用域调度的 `FS_CONTEXT.lock()`。这使得它无论当前进程在进入 syscall 处理了多深的锁递归链，都可以安全且非阻塞地将节点插入到 VFS 森林中。
* 这样完全遵守了内核高内聚低耦合的原则，按需加载层仅仅负责搬运二进制 `.ko` 和符号绑定，而文件树的管理无需进入 TLS （Thread Local Storage）上下文即可完成通信。

## 已知问题与根因分析 (非常重要)

在融合 `FuseUsageChecker` 和测试 FUSE 时，曾遭遇了极为隐蔽的内存异常与 Kernel Panic。排查明确以下根因：

### 1. `ext4_bcache_free` 内存重读崩溃（已修复）
早期版本的 `FuseUsageChecker` 在检查一个模块是否被占用时，对遍历到的每一个文件描述符使用了直接转路径的判断 `file.path().contains("fuse")`。
**根因**：调用 `.path()` 方法在面对底层驱动为 `ext4` 的文件时，会触发 Ext4 文件系统反向向磁盘块发起寻找与读流缓存的操作。由于 UsageChecker 是在背景时钟（Timer Callback）以及无预警作用域中频繁执行的，这打乱了正在运作的 Ext4 IO Cache Block 操作（如 `ext4_bcache_drop_buf`）锁机制，引起 Read Page Fault 宕机。
**修复办法**：代码被精修改为“两级拦截匹配”机制。即在调用 `.path()` 方法提取路径名之前，通过 `loc.filesystem().name()` 方法进行文件系统归属分类。只有在目标对象为不涉及磁盘 IO 的 `devfs`（纯内存树）时，才调用其 path 方法，其余一律只进行标识比对，彻底解决了系统颠簸。

### 2. `fuse_test` 中途引发 `procfs` 重入崩溃锁死（相关模块问题）
当标准库执行 `sys_mount` 或者 `create_dir_all` 探测运行时环境时（用户执行 `/musl/fuse_test` 的幕后行为），标准 libc 自动尝试访问 `/proc` 相关挂载。
由于 `procfs` 被设计为在按需加载后会强制跨边界读取主内核 `FS_CONTEXT.lock()` 宏。在主 syscall 回路已经锁定了一层资源的状态下，该跨模块的宏指针解析重定位错误地访问到了未知地址 `0x1a0bdce93a9ee37f`，引起崩溃。
这反面印证了 FUSE 按需加载所采取的 `静态内存表注册模式` (不干涉全局上下文) 是开发外部挂载模块最安全的标准途径。
## 本周核心代码变更总结 (2026年3月底)

近期关于 FUSE 按需加载的所有改动已全部落库，主要涵盖了依赖修正、外壳模块构建、注册机制与极其关键的文件系统类型安全检查，具体变更细节如下：

### 1. 工作空间依赖与构建梳理
- **Submodule 修正**：
  - 清理了意外引入的 `alloystack` 子模块，避免构建污染 (commit: `2ca5cc9`)。
  - 为修复 `fuse.ko` 链接问题，升级并同步了 `arceos` 子模块指针 (commit: `d4c5ff4`)。
- **构建系统集成**：在工作空间及构建脚本中增加规则，确保 `make modules` 能通过 Rust BPF/Kmod 工具链正确编译 `modules/fuse` 并最终输出 `/root/modules/fuse.ko`。

### 2. 构建独立的 FUSE 外壳模块 (`modules/fuse/`)
- 提供了标准的 `kmod` 生命周期函数包裹：
```rust
#[init_fn]
pub fn fuse_init() -> i32 {
    let ret = starryfuse::init_fuse();
    // ...
}

#[exit_fn]
fn fuse_exit() {
    starryfuse::exit_fuse();
}
```
- 将具体的 `devfs` 节点与协议管道注册完全交给下层 `starryfuse` 库实例。

### 3. StarryOS VFS 层按需加载触发入口注册 (`api/src/kmod/ondemand_builtin.rs`)
通过 `register_builtin_modules` 正式将 FUSE 接轨至 StarryOS On-Demand 框架：
```rust
super::ondemand::register_module(ModuleDesc {
    name: "fuse",
    ko_path: "/root/modules/fuse.ko",
    idle_timeout_ticks: 5_000,
    trigger: Box::new(PathPrefixTrigger::new("/dev/fuse")),
    usage: Some(Box::new(FuseUsageChecker)),
});
```

### 4. 修复 Ext4 缓存崩溃的 VFS 路径安全检查 (Usage Checker 核心改动)
这是近期最重要的代码修复。解决了老版本在全量轮询 FD 时粗暴调用 `.path()` 引发的 `ext4_bcache_free` 核级崩溃。修复后的检测器加入了底层挂载点分类校验，彻底化解了因为时钟中断导致的主存盘缓存读写冲突。
```rust
// api/src/kmod/ondemand_builtin.rs 最终版修复片段
if let Some(file) = any.downcast_ref::<crate::file::File>() {
    let loc = file.inner().location();
    let fs_name = loc.filesystem().name();
    
    if fs_name == "fuse" { return true; } 
    // 【核心修复点】: 仅在内存文件系统 devfs 才安全解包路径，避开磁盘 I/O 雷区
    else if fs_name == "devfs" {
        let fpath = crate::file::FileLike::path(file);
        if fpath == "/dev/fuse" || fpath == "/fuse" || fpath == "fuse" {
            return true;
        }
    }
}
```

### 5. 底层字符设备挂靠 (`Starryfuse/src/lib.rs`)
遵循微内核通信范式，不再读写主文件系统树根节点或宏变量。
```rust
// 通过轻量锁创建内存实例
let conn = FUSE_CONNECTION.get().cloned().unwrap_or_else(|| { ... });
// 直接下压至 VFS Table 进行挂考
register_devfs_device("fuse", NodeType::CharacterDevice, DeviceId::new(10, 229), Arc::new(FuseDev { conn }));
```

### 6. Starryfuse 独立协议核心库实现
将 FUSE 核心机制独立剥离为单独的 Crate (`Starryfuse/Cargo.toml`) 全新引入。主要覆盖以下代码：
- **`abi.rs`**：规范对齐了 Linux 标准中 FUSE 协议层的全部上下行（请求/响应）数据排布。
- **`dev.rs`**：专门实现了基于 `FuseDev` 抽象的通信设备节点出入队操作管理（`poll`, `read`, `write`）。
- **`vfs.rs`**：FUSE 层面定制的内挂虚文件系统结构和索引分配屏蔽接口。

### 7. 用户态 FUSE 守护进程测试联调 (`fuse_test`)
额外引入并集成运行了用户态 FUSE 后台测试应用，用于印证完整的交互闭环与稳定性：
- 测试驱动从用户态发起初始 `open("/dev/fuse")` 引发由 VFS Not Found 到内核执行 On-Demand LKM 模块加装的全过程。
- 确立了后台主态轮询事件循环读取 `fuse_in_header` 及相关特约请求 (`lookup`, `getattr` 等)。
- 将模拟好的结构回写确认，标志着 `StarryOS VFS -> Fuse 设备文件 -> Kernel Mod -> User Daemon` 从底至上的全功能通路终于完全通车。

### 8. VFS 动态文件系统注册机制与底层硬编码解耦 (当前状态核心改动)
主系统内核通过解耦改造，进一步去除了对特定文件系统的硬绑定，提供真正完善的动态化支持：
- **动态注册表 (`api/src/vfs/mod.rs`)**：新增了包含无锁映射表的 `FS_REGISTRY` 与对外接口 `register_filesystem` / `get_filesystem_creator`。这使得 `.ko` 动态外挂模块可以自行向 VFS 注册文件系统的创建闭包，而不是在内核写死。
- **`sys_mount` 改造 (`api/src/syscall/fs/mount.rs`)**：系统调用 `sys_mount` 在处理挂载时不再使用大量的 if-else 硬编码探测文件系统，而是通过动态匹配 `get_filesystem_creator(&fs_type)` 获取目标挂载逻辑。实现了对外部模块的零感知。
- **卸载特化逻辑清理 (`api/src/kmod/ondemand.rs`)**：彻底移除了按需加载器内部针对 `procfs` 硬编码的 "先 unmount 再立即 delete 模块" (Two-phase unload) 特化脏代码。让 `ondemand` 管理器回归纯理性的通用生命周期管控，杜绝模块特权耦合。



## Issue #2 解决历程

这一部分详细记录了我们在对齐远程 723fc6b 版本与当前本地最终可工作版本之间的过程。

底层 panic -> 逻辑层挂起阻塞 (卡死) -> 资源层异常反馈 -> 平稳回收释放

以下是基于 `723fc6b` 的完整改动总览。

| 故障类别 | 直接现象 | 根本原因 (Root Cause) | 关键修改文件 | 修复方案 | 结果 |
|---|---|---|---|---|---|
| **P1: 模块代码段的 Use-After-Free** | 动态卸载模块后，系统抛出 `Unhandled Supervisor Page Fault (EXECUTE)` 内核异常。 | VFS 或相关子系统中仍遗留着指向该模块代码段 (`.text`) 的入口指针。模块内存释放后，若控制流继续调用该悬空指针（Dangling Pointer），会触发指令级缺页（Execute Page Fault）。 | `api/src/kmod/ondemand.rs`<br>`modules/fuse/src/lib.rs`<br>`api/src/vfs/mod.rs` | **1. 强化卸载条件：** 仅当引用计数为零且无被打开的 `/dev/fuse` FD 时才允许卸载。<br>**2. 清除悬空指针：** 在 `fuse_exit()` 中彻底注销所属设备节点和文件系统资源。 | 彻底消除了执行型缺页引发的 Kernel Panic，动态加载/卸载流程安全闭环。 |
| **P2: 守护进程 IO 挂起 (Daemon Hang)** | 测试结束后用户态守护进程一直挂起，`/dev/fuse` FD 被持续占用，致使内核模块无法达成卸载条件。 | 守护进程的主事件循环使用了同步阻塞型的 `read()` 系统调用。当内核不再发送 FUSE 请求时，进程永久死锁在此 IO 等待队列中，无法释放 FD。 | `Starryfuse/tests/fuse_test/src/main.rs` | 引入 IO 多路复用机制，使用带超时的 `libc::poll()` 替代阻塞式的 `read()`。当侦测到超时即代表任务结束，此时主动跳出循环并完成清理退出。 | 守护进程通过超时机制确认通信结束，正常主动退出并归还了 FD 资源控制权。 |
| **P3: 目录读取活锁 (Readdir Livelock)** | 底层日志被高频循环触发的 `FUSE_READDIR`（opcode=28）事件霸占。 | 用户态模块未遵守文件读取游标 (`offset`) 语义，无视进度始终回复首个目录项。这导致内核 VFS 一直获取不到文件尾 (EOF) 标志，陷入不断重推读取命令的活锁机制。 | `Starryfuse/tests/fuse_test/src/main.rs` | 补充游标状态机规则：当收到 `offset != 0` 的请求时，直接回复大小为 0 的空结构(Empty Payload)，以此向内核传递标准的 EOF 信号。 | VFS 明确接收了已达文件末尾的信号，终止检索并跳出了无穷递归死循环。 |
| **P4: 字符设备 IO 阻塞语义缺失** | 该设备在被内核调度器的 `poll`/`select` 机制监管时，暴露出了不稳定的同步唤醒与挂断行为。 | `/dev/fuse` 的驱动在呈现节点配置时，并未向内核显式注册自身等同于字符设备的阻塞属性。 | `Starryfuse/src/dev.rs` | 重写设备接口 `flags()` 方法，显式向内核 VFS 抛出 `NodeFlags::BLOCKING` 特征。 | 使内核进程调度器清楚感知该节点的阻塞特征，其对应的多路复用调度表现回归正常。 |
| **P5: VFS 元数据更新引发异常告警** | 当触发虚拟文件系统的节点注销时，系统高频打印 `Failed to update file times on drop: OperationNotSupported`。 | VFS 在析构内存 inode 时，默认调用底层的 `update_metadata()` 试图更新其访问时间戳。该虚拟中间层缺乏此针对性支持而上报了失败异常。 | `Starryfuse/src/vfs.rs` | **实现最佳兼容的元数据更新接口：** 手动装载 `update_metadata()` 接口函数，暂时静默时间更新命令并抛出代表合法的 `Ok(())` 状态。 | 在迎合上级 VFS 的通用销毁框架下，阻拦并屏蔽了这些徒劳的磁盘同步告警。 |
| **P6: 后台监控引发 Ext4/procfs 内核崩溃** | 后台定时检查模块是否空闲时，系统有时会爆发 `ext4_bcache_free` 等与 FUSE 完全无关的内核崩溃。 | 检查器粗暴遍历了系统所有打开的文件并强取路径 (`.path()`)。当时钟中断强制获取 Ext4 磁盘文件或 procfs 文件的路径时，违规触发了底层磁盘缓存 IO 或死锁。 | `api/src/kmod/ondemand_builtin.rs` | **缩紧检查目标**：先通过 `filesystem().name()` 确认文件类型，只要不是内存设备 (`devfs`) 则绝对不调取路径，避开磁盘 IO 和敏感锁。 | 后台检查逻辑不再误伤常规系统文件，消除了系统随机崩溃。 |

以下是 Issue #2 核心问题的完整排查与解决链路：

### Issue #2 问题的排查与解决过程

#### 第一阶段：解决模块卸载后的 Use-After-Free 问题引发的页错误

**故障现象：**
测试日志显示，可加载内核模块 (LKM) 在卸载后，系统发生 `Unhandled Supervisor Page Fault (EXECUTE)` 内核异常。因为程序计数器 (`sepc`) 指向了已经被释放的模块 `.text` 代码段，最终导致 Kernel Panic。

**根因分析：**
原因是模块释放时，内核中仍有残留的指针指向该模块。当系统尝试通过这些指针执行已经被回收的内存页时，触发了缺页异常。

**修复措施：**
* **增加卸载条件检查：**在卸载逻辑中增加前置判断，只有当模块的引用计数归零，且没有活动的 `/dev/fuse` 文件描述符 (FD) 时，才允许卸载模块。
* **清理注册资源：**在模块的退出函数 `fuse_exit()` 中调用 `unregister_devfs_device` 和 `unregister_filesystem`，确保清理设备和文件系统的注册信息。
* **优化状态检查：**修改 `FuseUsageChecker` 的逻辑，准确获取模块当前的占用状态，防止因判断错误导致模块被提前卸载。

#### 第二阶段：解决守护进程的 I/O 阻塞挂起 (Daemon Hang)

**故障现象：**
内核 Panic 解决后，客户端测试在结束时，用户态守护进程 (Daemon) 挂起无法退出。这导致 `/dev/fuse` 被持续占用，LKM 模块也因为不满足空闲条件而无法自动卸载。

**根因分析：**
守护进程使用阻塞式的 `read` 系统调用来等待内核的 FUSE 请求。测试结束后，内核不再下发新请求，守护进程就会一直阻塞在 `read` 调用上，无法执行后续的清理和退出逻辑。

**修复措施：**
* **使用 I/O 多路复用机制：**将守护进程的阻塞式 `read()` 改为带有超时机制的 `libc::poll()`。如果在设定的时间内没有收到内核的新请求，就判定测试结束。此时守护进程主动退出并释放相关的资源。

#### 第三阶段：解决 readdir 处理不当导致的死循环

**故障现象：**
在执行 `ls` 等读取目录的测试时，系统不断产生 `FUSE_READDIR`（Opcode 28）事件，导致测试陷入死循环。

**根因分析：**
守护进程在处理 `READDIR` 请求时，忽略了 FUSE 协议中的读取偏移量（`offset`）参数，总是返回第一个目录项。因为内核 VFS 收不到代表目录读取结束的标志（EOF），所以只能不断增加 `offset` 并重新发起请求，从而产生死循环。

**修复措施：**
* **完善偏移量校验：**修改 `handle_readdir()` 的处理逻辑。当收到 `offset != 0` 的请求时，直接返回一个大小为 0 的空数据。这相当于向内核发送了 EOF 信号，VFS 收到后就会确认目录读取完毕，从而结束循环。

#### 第四阶段：支持 VFS 元数据更新并消除错误告警

**故障现象：**
在进程关闭文件描述符触发 VFS 节点清理时，内核频繁打印警告信息：“`Failed to update file times on drop: OperationNotSupported`”。

**根因分析：**
内核 VFS 在释放 Inode 之前，默认会调用 `update_metadata()` 来更新文件的创建或访问时间。因为初期的 FUSE 实现中没有对接这个操作，VFS 发现操作不被支持后，抛出了系统警告。

**修复措施：**
* **实现空的元数据兼容接口：**在虚拟层实现了 `update_metadata()` 接口。由于目前的测试守护进程还未实现 `FUSE_SETATTR` 的完整对接，内核虚拟层在收到 `atime/mtime` 时间更新请求时，直接返回 `Ok(())` 状态妥协。这样满足了 VFS 必须成功调用的强制要求，彻底消除了这些挂载清理期间的告警信息。

### 总结

针对 commit 723fc6b 版本的 Issue #2，我们依次修复了模块释放后的缺页错误、守护进程阻塞退出、readdir 目录读取死循环以及元数据更新告警等问题。这些修复使 FUSE 机制能够在系统的动态加载模块 (LKM) 体系中稳定、正常地运行。

### 附录：核心变更文件的历史修改演进

本部分列出从 commit 723fc6b 到当前版本，各个核心文件在修复系统崩溃、阻塞、死循环及功能补全过程中的具体代码变动。

#### 1. Starryfuse/src/vfs.rs (FUSE 虚拟文件系统适配层)

该文件作为 FUSE 与 StarryOS VFS 的适配层，经历了如下修改：

**第一版本 (commit 723fc6b)：**
- `update_metadata()` 方法缺失，时间戳等操作默认返回 `OperationNotSupported` 错误。

**第二版本（将初始化改为异步）：**
- 原版的 FUSE INIT 握手过程为阻塞式同步实现。
- 当主内核锁与模块初始化锁存在等待依赖时会导致阻塞。
- 修改为使用 `axspawn::spawn()` 异步执行握手请求。

**第三版本（支持元数据更新补偿防告警）：**
- 实现 `update_metadata()` 接口应对 VFS 节点释放时的属性下发。当检测到 `mode` 与 `owner` 被修改时保留 `OperationNotSupported` 报错。
- 单独为基于 `atime` 与 `mtime` 修改时间戳发起的 `metadata` 更新请求返回了一个空的 `Ok(())`状态。这是一个临时的补偿接口，用于消除 FUSE 未实现全套 `FUSE_SETATTR` 时出现的节点挂载与销毁告警。

#### 2. Starryfuse/tests/fuse_test/src/main.rs (用户态 FUSE 守护进程)

该测试守护进程经过了如下修改：

**第一版本 (commit 723fc6b)：**
- 主事件循环使用阻塞式 `read()` 系统调用读取内核请求。
- 测试程序结束后，守护进程会停留在 read() 阻塞中无法退出，持续占用 FD 导致 LKM 无法自动卸载。
- 目录读取 (`handle_readdir`) 未处理 offset 游标参数，持续返回同一项内容导致内核死循环发起请求。

**第二版本（引入测试子进程监控）：**
- 增加了 `build marker` 版本标识。
- 使用 `fork()` 创建子进程执行文件系统测试，父进程负责监控子进程的退出状态 (`exit status`)，以确认真实执行结果。

**第三版本（替换阻塞 I/O 为轮询）：**
- 将事件循环中的阻塞 `read()` 替换为 `libc::poll()` 分发，设置 30 秒超时时间。
- 当 poll 返回超时或等待出错时，判定测试结束，退出循环并清理资源。
- 代码片段示例：
  ```rust
  let mut poll_fds = [libc::pollfd {
      fd: fuse_fd,
      events: libc::POLLIN,
      revents: 0,
  }];
  
  loop {
      let poll_ret = unsafe { libc::poll(poll_fds.as_mut_ptr(), 1, 30_000) };
      if poll_ret <= 0 { break; }  // 超时或出错则退出
      // 处理 POLLIN 事件
  }
  ```

**第四版本（修复目录读取的死循环）：**
- 在 `handle_readdir()` 函数增加 offset 参数检查。
- 当 offset != 0 时，直接回复空负载数据（代表目录读取结束的 EOF），使系统退出循环读取。
- 代码片段示例：
  ```rust
  fn handle_readdir(&mut self, unique: u64, nodeid: u64, offset: u64) {
      if offset != 0 {
          self.send_response(unique, &[]);  // EOF
          return;
      }
      // 返回第一段目录项
  }
  ```

**第五版本（响应元数据更新）：**
- 配合内核层支持，在主循环中增加了对 `FuseOpcode::Setattr` (Opcode 4) 的支持。
- 新增 `handle_setattr()` 方法，接收到属性修改请求后，返回对应的 `FuseAttrOut` 结构体完成确认。

#### 3. Starryfuse/src/abi.rs (FUSE 协议结构定义)

**支持文件属性修改：**
- 新增了 `FuseSetattrIn` 结构体，用于映射 Linux FUSE ABI 中设定文件属性的下发负载。
- 定义了配套的 `FATTR_UID`, `FATTR_GID`, `FATTR_ATIME`, `FATTR_MTIME` 等位段宏常量。

#### 4. Starryfuse/src/dev.rs (FUSE 虚拟字符设备节点)

**补充设备特征标识：**
- 原版中未定义该字符设备的阻塞特性。
- 修改 `flags()` 方法，显式返回 `NodeFlags::BLOCKING`。
- 让内核在处理 poll/select 时能够正确判断该设备的等待属性。
- 代码片段示例：
  ```rust
  impl DeviceOps for FuseDev {
      fn flags(&self) -> NodeFlags {
          NodeFlags::BLOCKING
      }
  }
  ```

#### 5. api/src/vfs/mod.rs (VFS 文件系统管理)

**补充文件系统注销接口：**
- 新增 `unregister_filesystem(name: &str) -> bool` 函数。
- 模块卸载时，用于从 `FS_REGISTRY` 映射表中主动移除模块曾注册的文件系统类型。
- 代码片段示例：
  ```rust
  pub fn unregister_filesystem(name: &str) -> bool {
      FS_REGISTRY.remove(name).is_some()
  }
  ```

#### 6. modules/fuse/src/lib.rs (FUSE 可加载模块入口)

**完善卸载清理操作：**
- 在 `#[exit_fn]` (模块卸载钩子) 中显式调用注销接口：
  1. `unregister_devfs_device("fuse")` (注销设备节点)
  2. `unregister_filesystem("fuse")` (注销文件系统类型注册)
  3. `starryfuse::exit_fuse()` (运行后续通用清理)
- 避免了模块内存释放后遗留可达引用引发问题的可能。
- 代码片段示例：
  ```rust
  #[exit_fn]
  fn fuse_exit() {
      let _ = crate::api::vfs::unregister_devfs_device("fuse");
      let _ = crate::api::vfs::unregister_filesystem("fuse");
      starryfuse::exit_fuse();
  }
  ```

#### 7. api/src/kmod/ondemand_builtin.rs (按需模块注册与资源监控)

**修复读路径引发的内核崩溃：**
- 早期版本的 `FuseUsageChecker` 判断模块是否被占用时，会默认调用遍历到的文件句柄的 `.path()` 方法。在时钟中断的上下文中，对 Ext4 等磁盘文件系统调用该方法触发了底层缓存资源冲突（如 `ext4_bcache_free` panic）。
- 修改逻辑：先检查文件系统名称（`filesystem().name()`）。只有目标为内存文件系统 `devfs` 时，才提安全取路径提取并比对 `/dev/fuse`，消除了读取磁盘路径相关的随机崩溃。

#### 8. api/src/kmod/ondemand.rs (按需加载框架管理)

**细化卸载前置检查：**
- 进一步完善了模块尝试卸载时的依赖检查。
- 只有同时满足引用计数归零与 Usage Checker 判定空闲，模块才能进入内存释放流程，杜绝释放后再次被执行造成的缺页错误。

### 演进总结

以上修改使得代码能够避免异常崩溃、解决守护进程阻塞、处理目录读取死循环、并支持元数据的顺利更新。目前 FUSE 模块加载、功能通信和自动卸载的完整生命周期均已经能正常跑通。

### 附录 2：`fuse_test` 完整执行日志与流程对照解析

用户态测试守护进程的主跑日志展示了 FUSE 模块在按需加载状态下完美的“创建、解析、交互、销毁”控制流交互。以下是对该输出节点的逐行原理解析：

```text
Opened /dev/fuse
```
**原理解析：** 用户态 Daemon 首次尝试开启虚拟字符设备 `open("/dev/fuse")`。在此瞬间，由于系统尚未加载 FUSE 驱动，VFS 捕获 NotFound 并触发 `on-demand` 机制，将 `fuse.ko` 动态装入内核内存，将驱动节点接入 `devfs`。随后 `open` 顺利返回文件句柄。

```text
Mounted /mnt/fuse successfully
```
**原理解析：** Daemon 调用 `sys_mount` 或者 `mount`，将刚刚取得的 FUSE 通信隧道挂接至全系统的 `/mnt/fuse` 目录。挂载操作激活了 VFS 层的桥接，之后对该目录下的查询都会被拦截塞往 Daemon 的通道中。

```text
About to fork self-test child...
fork returned 13
Spawned self-test child pid=13
fork returned 0
=== FUSE Self-Test Starting ===
```
**原理解析：** 程序在代码里执行 `fork()` 分化出一个普通子进程（本例中系统的 PID 分配为 `13`）。
- **守护（父）进程（PID > 0）** 会进入后台事件循环，专心从 `/dev/fuse` 听取内核请求并抛出响应；
- **子进程（PID=0）** 作为“客户端”去模拟普通用户的访问行为，开始进犯 `/mnt/fuse` 目录做测试。

```text
Received FUSE request: opcode=26, unique=1, nodeid=0
Sent INIT response
```
**原理解析：** **协议初始化握手（`FUSE_INIT`，Opcode=26）**。这是底层 FUSE 挂载成功后第一次强制对话。内核汇报其兼容的缓冲区及版本约束（`max_readahead`等），Daemon 收信答复，双边长连接正式确认。

```text
[TEST] ls /mnt/fuse:
Received FUSE request: opcode=28, unique=2, nodeid=1
Sent READDIR response (offset=0, bytes=96)
  test.txt
Received FUSE request: opcode=28, unique=3, nodeid=1
Sent READDIR response (offset=3, bytes=0)
[TEST] ls /mnt/fuse: PASS
```
**原理解析：** 这是客户端做了一次**目录枚举扫描 (即 `ls`)**：
1. 子进程调用底层的 `getdents64`，传导至 Daemon 时便是请求列出根目录内容，即 **`READDIR` (Opcode=28，`nodeid=1` 指代根节点)**，游标 `offset=0`。Daemon 用 96 个字节的信息组装出了一个文件名 `test.txt` 发还内核，这使得终端成功打印。
2. VFS 层收到后，尝试“是不是还有剩下的项目？”，向后挪动游标并再发一次 `READDIR` (`offset=3` 即刚才回复记录结束点)。Daemon 程序据此返回空字节长度 (`bytes=0`) 以**表明已是目录末尾 (EOF)**。`ls` 命令完满跳出，打印 `PASS`。

```text
Received FUSE request: opcode=1, unique=4, nodeid=1
Sent LOOKUP response for 'test.txt'
```
**原理解析：** 子程序意图去读 `test.txt` 了。在真的获取数据之前，VFS 核心需要先向后台定位验证该字符串对象是否真实存在，因此它在父节点 `nodeid=1` 下发起 **`LOOKUP` (Opcode=1)** "test.txt"。服务进程告知：确有此物，且它的内部标识 ID 叫做 `nodeid=100`。

```text
Received FUSE request: opcode=3, unique=5, nodeid=100
Sent GETATTR response for nodeid=100
... (连续多次 op=3) ...
```
**原理解析：** 拿到节点标号后，应用程序所引用的高级标准库常会在触发真正读写前多次轮询 **`GETATTR` (即底层 `stat/fstat` 的 FUSE 对应行为，Opcode=3)**，向我们要该标号 `nodeid=100` 的元数据（文件大小尺寸、权限等）以开辟缓冲区和做权限安全确认。

```text
Received FUSE request: opcode=15, unique=8, nodeid=100
Sent READ response (nodeid=100, offset=0, req_size=4096, bytes=13)
```
**原理解析：** 这是核心的文件数据抓取阶段，**系统内核向我们要货（`READ`，Opcode=15）**。请求内核要从第0个偏移量开始最高吸取 `4096` 字节的内容。我们的测试驱动按设定回应了长度刚好为 `13` 字节的内容块：`"hello, fuse!\n"`。

```text
[TEST] read test.txt: PASS (contents: "hello, fuse!\n")
=== FUSE Self-Test Complete ===
Self-test child exited, status=0
```
**原理解析：** 子进程在成功并无错乱地收到内核传过来的 13 字节后，确认本次文件系统完整穿越没有崩溃，打印出字符串，随后自我回收调用 `_exit(0)` 主动挂掉死亡。

```text
Test complete, daemon exiting.
```
**原理解析：** 父进程的主循环挂置在有超期的 `libc::poll` 陷阱内。此时它通过系统调用发现了监控的子进程已经被回收 (`waitpid` status=0)，且在规定的静默倒计时期限内，`/dev/fuse` 通道里毫无请求回音冒出。
这使得挂壁测试环境正常落幕：它安全地打断事件流，全数退出并关闭程序。紧随其后地解除了对相关系统 FD 的常驻占据，为随后 StarryOS 核心时钟清理 `fuse.ko` 模块按需卸载任务交出了所有的前置环境票根！

## 运行结果

```bash
[  5.774269 0:11 starry_api::kmod::ondemand:43] [ondemand] module 'fuse' loaded, handle=0x17c96ff18
Opened /dev/fuse
Mounted /mnt/fuse successfully
About to fork self-test child...
fork returned 13
Spawned self-test child pid=13
Received FUSE request: opcode=26, unique=1, nodeid=0
Sent INIT response
fork returned 0
=== FUSE Self-Test Starting ===
[TEST] ls /mnt/fuse:
Received FUSE request: opcode=28, unique=2, nodeid=1
Sent READDIR response (offset=0, bytes=96)
  test.txt
Received FUSE request: opcode=28, unique=3, nodeid=1
Sent READDIR response (offset=3, bytes=0)
[TEST] ls /mnt/fuse: PASS
Received FUSE request: opcode=1, unique=4, nodeid=1
Sent LOOKUP response for 'test.txt'
Received FUSE request: opcode=3, unique=5, nodeid=100
Sent GETATTR response for nodeid=100
Received FUSE request: opcode=3, unique=6, nodeid=100
Sent GETATTR response for nodeid=100
Received FUSE request: opcode=3, unique=7, nodeid=100
Sent GETATTR response for nodeid=100
Received FUSE request: opcode=15, unique=8, nodeid=100
Sent READ response (nodeid=100, offset=0, req_size=4096, bytes=13)
Received FUSE request: opcode=3, unique=9, nodeid=100
Sent GETATTR response for nodeid=100
[TEST] read test.txt: PASS (contents: "hello, fuse!\n")
=== FUSE Self-Test Complete ===
Self-test child exited, status=0
Test complete, daemon exiting.
starry:~# [ 11.153416 0:6 starry_api::kmod::ondemand:55] [ondemand] unload handle=0x17c96ff18
[ 11.154924 0:6 kmod_loader::loader:122] Calling module exit function...
[ 11.156977 0:6 fuse:53] Fuse module exit called.
[ 11.157721 0:6 starry_api::kmod:179] Module(fuse) exited
[ 11.158666 0:6 starry_api::kmod:74] KmodMem::drop: Deallocating paddr=PA:0x819af000, num_pages=9
[ 11.160802 0:6 starry_api::kmod:74] KmodMem::drop: Deallocating paddr=PA:0x819b8000, num_pages=5
[ 11.161658 0:6 starry_api::kmod:74] KmodMem::drop: Deallocating paddr=PA:0x819bd000, num_pages=1
[ 11.162995 0:6 starry_api::kmod:74] KmodMem::drop: Deallocating paddr=PA:0x819be000, num_pages=1
exit
make[1]: Leaving directory 
```

## 架构优化进阶

### 字符设备的异步 I/O (Poller / Waker) 支持完成

在早期的设计中，为了绕开 `read("/dev/fuse")` 导致的线程挂起死锁，我们曾在 `FuseDev` 里通过 `NodeFlags::BLOCKING` 做了直接的同步阻塞处理。目前，该部分已经被重构，彻底改为了通过 `axpoll` 与内核调度层直接对接的事件驱动模式。

**重构内容与原理**：

1. **引入 PollSet 作为调度中心**：
   在 `Starryfuse/src/dev.rs` 中，我们为底层的 `FuseConnection` 增加了 `poll_set: PollSet` 以作为内核独立的等待队列。过去使用的强制阻塞标志 `NodeFlags::BLOCKING` 已被彻底清理，恢复为非阻塞标准位 `NodeFlags::empty()`，并通过覆盖 `as_pollable` 接口允许内核的多路复用器接管该设备。

2. **完整接入 Pollable 事件处理**：
   为 `FuseDev` 正式实现了 `axpoll::Pollable` 接口：当检查请求队列时，如果有待处理的数据则上报 `IoEvents::IN` 读状态；否则，就把当前调用读取的用户态任务通过 `register` 方法安全地寄存入休眠队列排队。

3. **内核下达请求时的安全唤醒**：
   在 VFS 层下发请求命令的关键路径（`Starryfuse/src/vfs.rs`）中，当新的任务指令投入队列时会立刻触发 `conn.poll_set.wake()`。这会自动拉起休眠中的处理程序立即开始工作。

**实际收益**：

- **解除进程读取瓶颈**：当用户态调用请求且没有任何回音时，进程不再强制卡死挂起。调度器会自动挂断并让出 CPU 资源。
- **支持高并发与事件循环**：用户态通信程序目前已可以通过 `epoll` 或 `poll` 等事件驱动函数去同时监听多个 FUSE 挂载点及其他网络连接。这让守护系统拥有了正常、高并发的事件循环逻辑底座。


## 最新改动记录 

> 本节汇总自 `4ac40dd`（异步 Poller 支持完成）之后至当前 HEAD 的全部 FUSE 按需加载相关改动。

### 改动总览

这段时间的改动可归纳为四条主线：

1. **I/O 模型**：从"非阻塞轮询"演进为"锁外阻塞睡眠"。`WaitQueue` 让守护进程为空时安全挂起；后续锁结构重构将 `WaitQueue` / `PollSet` 移出自旋锁保护范围，彻底消除"持锁睡眠"死锁隐患。

2. **VFS 能力**：从基础 `read`/`write` 扩展为支持 `symlink` / `link` / `rename` / `mkdir` / `truncate` 的完整子集。

3. **测试矩阵**：从单一 `fuse_test` 扩展为三级验证——`fuse_rw_test`（综合功能）、`fuse_mem_test`（内存量化），且 Makefile 支持一键更新 QEMU 磁盘镜像。

4. **内存可观测性**：从零散日志升级为结构化剖析框架。`OndemandMemInfo` 在 `load` / `unload` 四个时间点采集 `RustHeap` 与物理页双维度快照，通过 `/proc/ondemand_meminfo` 导出，证明按需加载可节省约 **1 MB** 常驻内存。

---

### 1. WaitQueue 阻塞式 read 与连接生命周期管理

**最终设计**：`FuseConnection` 同时持有 `WaitQueue`（`read` 阻塞）和 `PollSet`（`poll` / `epoll`），二者均位于 `SpinNoIrq<FuseConnectionState>` 之外。守护进程调用 `read` 时若队列为空，内核将当前任务挂起，直到新请求到达或连接被终止。

`read_at` 的核心逻辑是"检查 → 释放锁 → 睡眠"三阶段循环：

```rust
loop {
    let mut state = self.conn.state.lock();
    if state.aborted { return Ok(0); }
    if let Some(req) = state.pending.pop() {
        // 移入 processing 并返回数据
        return Ok(total_len);
    }
    drop(state);  // 关键：睡眠前必须释放锁

    self.conn.wait_queue.wait_if(1, None, || {
        let s = self.conn.state.lock();
        s.pending.is_empty() && !s.aborted
    })?;
}
```

当模块卸载时，`exit_fuse` 设置 `aborted = true` 并批量唤醒所有等待线程，确保守护进程干净退出：

```rust
conn.state.lock().aborted = true;
conn.wait_queue.wake(usize::MAX, 1);
conn.poll_set.wake();
```

同时 `Pollable::poll` 在 `aborted` 状态下上报 `HUP | ERR`，让使用 `epoll` 的守护进程也能感知连接关闭。

---

### 2. VFS 扩展：symlink、link、rename

**最终设计**：ABI 层新增 `FuseRenameIn` 与 `FuseLinkIn`；VFS 层实现 `set_symlink`、`link`、`rename`。

`rename` 的 in_data 格式为 `[FuseRenameIn][old_name\0][new_name\0]`，其中 `newdir` 字段存放目标目录的 nodeid：

```rust
fn rename(&self, old_name: &str, target: &DirNode, new_name: &str) -> VfsResult<()> {
    let mut in_data = /* FuseRenameIn 二进制 */;
    in_data.extend_from_slice(old_name.as_bytes()); in_data.push(0);
    in_data.extend_from_slice(new_name.as_bytes()); in_data.push(0);
    self.fs.send_request(FuseOpcode::Rename, self.nodeid, in_data)?;
    Ok(())
}
```

`link` 返回的 `FuseEntryOut` 中通过 `mode & 0o170000` 判断文件类型，据此构造 `RegularFile` 或 `Directory` 的 `DirEntry`。

---

### 3. FuseConnection 锁结构重构（关键死锁修复）

**最终设计**：将"受锁保护的状态"与"并发安全的同步原语"分离为两层。

```rust
pub struct FuseConnectionState {
    pub pending: Vec<Arc<SpinNoIrq<FuseRequest>>>,
    pub processing: BTreeMap<u64, Arc<SpinNoIrq<FuseRequest>>>,
    pub aborted: bool,
}

pub struct FuseConnection {
    pub state: SpinNoIrq<FuseConnectionState>,
    pub poll_set: PollSet,         // 独立，无锁
    pub wait_queue: WaitQueue,     // 独立，无锁
}
```

`FuseDev` 的类型从 `Arc<SpinNoIrq<FuseConnection>>` 简化为 `Arc<FuseConnection>`。操作 `pending` / `processing` 时只短暂持有 `state` 锁；`PollSet` 的注册与 `WaitQueue` 的等待完全无锁。

这一重构还带来额外收益：`Pollable::register` 不再需要先 `lock()`，可直接注册 waker。

---

### 4. 三级测试矩阵与构建流程

**最终状态**：三个测试程序 + Makefile `disk` 目标一键更新 QEMU 镜像。

| 测试程序 | 覆盖范围 |
|---------|---------|
| `fuse_test` | 基础 `read` / `write` / `lookup` / `readdir`，验证按需加载最小闭环 |
| `fuse_rw_test` | 读写已有文件、创建新文件、`mkdir`、跨目录 `readdir`、`truncate` |
| `fuse_mem_test` | 触发 load → 自测 → umount → 等待 idle unload → 读取 `/proc/ondemand_meminfo` 并输出内存分析报告 |

`Makefile` 的 `disk` 目标自动挂载 `arceos/disk_$(ARCH).img`，用 `sudo cp -f` 安装二进制，再卸载，确保 QEMU 启动时拿到的是最新测试程序。

---

### 5. 内存剖析框架与 ABI / VFS 修正

#### 5.1 内存可观测性

`api/src/kmod/ondemand.rs` 中 `ModuleLoader::load` / `unload` 在四个时间点调用 `log_heap`，同时追踪 `RustHeap`（Rust 全局分配器）与 `used_pages`（页分配器），因为 `.ko` 的代码页通过 `vmalloc` / `dealloc_frames` 分配，不经过 Rust 堆：

```rust
fn log_heap(tag: &str) {
    let heap = axalloc::global_allocator().usage_stats()
        .get(axalloc::UsageKind::RustHeap) as u64;
    let pages = axalloc::global_allocator().used_pages() as u64;
    // 根据 tag 存入 ONDEMAND_MEM 对应字段
}
```

`api/src/vfs/proc.rs` 新增 `/proc/ondemand_meminfo` 节点，纯文本输出 8 组快照。`fuse_mem_test` 守护进程在卸载并等待 7 s 超时后读取该文件，生成内存分析报告。实测静态基线为 `fuse.ko` (416 KB) + `starryfuse` 库 (655 KB) = **1071 KB**，卸载后常驻 footprint 接近 0。

#### 5.2 ABI 修正

`FuseOpcode::Create` 从 `34`（实为 `FUSE_BMAP`）修正为 `35`；新增 `Access = 34`、`FATTR_SIZE` 常量与 `FuseSetattrIn` 结构体，补齐标准 ABI。

#### 5.3 VFS 行为修正

| 问题 | 最终处理 |
|------|---------|
| `open(O_CREAT)` 失败 | `update_metadata` 全字段 no-op（`axfs-ng-vfs` 在 `create_locked` 后无条件调用，返回 `OperationNotSupported` 会破坏创建流程） |
| VFS 页缓存对虚拟 inode 做预读/回写 | `FuseNode` 增加 `NON_CACHEABLE` 标志 |
| 文件截断无内核支持 | `set_len` 通过 `FUSE_SETATTR` + `FATTR_SIZE` 转发给用户态 |
| `create` 不支持目录 | 按 `NodeType` 分发：`RegularFile` → `FUSE_CREATE`，`Directory` → `FUSE_MKDIR` |

---

### 6. 多进程卸载安全

**最终设计**：`ondemand_builtin.rs` 中 `FuseUsageChecker::prepare_unload` 先遍历**所有进程的作用域（process scope）**，逐个对 `/mnt/fuse` 执行 `unmount`，然后再做全局卸载。这避免了 `fork` 后子进程挂载点泄漏导致的卸载失败。

```rust
for task in tasks() {
    let scope = task.as_thread().proc_data.scope.read();
    let fs = FS_CONTEXT.scope(&scope).lock();
    if let Ok(loc) = fs.resolve("/mnt/fuse") {
        if loc.is_root_of_mount() { let _ = loc.unmount(); }
    }
}
// 然后再执行全局卸载
```

---

### 7. procfs 策略调整

为避免 procfs 频繁的加载/卸载噪声污染页分配器基线，procfs 从按需加载注册表中移除，恢复为**内核静态挂载**。这使得 FUSE 内存测试获得了稳定的对照基线。

---
