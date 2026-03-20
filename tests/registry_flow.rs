// cargo test --test registry_flow

use core::sync::atomic::{AtomicUsize, Ordering};

use ondemand_kmod::{
    AccessEvent, AccessResult, LoadError, ModuleDesc, ModuleLoader, ModuleRegistry,
    PathPrefixTrigger, UnloadError,
};

struct MockLoader {
    _dummy: (),
}

static LOADS: AtomicUsize = AtomicUsize::new(0);
static UNLOADS: AtomicUsize = AtomicUsize::new(0);

impl MockLoader {
    fn new() -> Self {
        Self { _dummy: () }
    }

    fn loads(&self) -> usize {
        LOADS.load(Ordering::Relaxed)
    }

    fn unloads(&self) -> usize {
        UNLOADS.load(Ordering::Relaxed)
    }
}

impl ModuleLoader for MockLoader {
    fn load(&self, _name: &str, _ko_path: &str) -> Result<u64, LoadError> {
        let n = LOADS.fetch_add(1, Ordering::Relaxed) as u64;
        Ok(n + 1)
    }

    fn unload(&self, _handle: u64) -> Result<(), UnloadError> {
        UNLOADS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn reset_counters() {
    LOADS.store(0, Ordering::Relaxed);
    UNLOADS.store(0, Ordering::Relaxed);
}

#[test]
fn load_on_access_and_unload_after_idle_timeout() {
    reset_counters();
    let registry = ModuleRegistry::new(MockLoader::new());
    let loader = MockLoader::new();

    assert!(registry.register(ModuleDesc {
        name: "procfs",
        ko_path: "/root/modules/procfs.ko",
        idle_timeout_ticks: 10,
        trigger: Box::new(PathPrefixTrigger::new("/proc")),
        usage: None,
    }));

    assert_eq!(
        registry.on_access(&AccessEvent::Path("/proc/meminfo"), 1),
        AccessResult::Loaded
    );
    assert_eq!(loader.loads(), 1);

    registry.tick(2);
    assert_eq!(loader.unloads(), 0);

    registry.tick(20);
    assert_eq!(loader.unloads(), 1);

    assert_eq!(
        registry.on_access(&AccessEvent::Path("/proc/cpuinfo"), 21),
        AccessResult::Loaded
    );
    assert_eq!(loader.loads(), 2);
}

#[test]
fn no_match_does_not_load_module() {
    reset_counters();
    let registry = ModuleRegistry::new(MockLoader::new());
    let loader = MockLoader::new();

    assert!(registry.register(ModuleDesc {
        name: "procfs",
        ko_path: "/root/modules/procfs.ko",
        idle_timeout_ticks: 10,
        trigger: Box::new(PathPrefixTrigger::new("/proc")),
        usage: None,
    }));

    assert_eq!(
        registry.on_access(&AccessEvent::Path("/dev/null"), 1),
        AccessResult::NoMatch
    );
    assert_eq!(loader.loads(), 0);
    assert_eq!(loader.unloads(), 0);
}
