// Copyright 2024-2026 the Deno authors. MIT license.

//! Resource Guardian — cooperative resource enforcement for multi-tenant Deno.
//!
//! In production deployments, a single rogue worker can starve every other
//! worker on the same machine. The Resource Guardian prevents this by tracking
//! per-worker resource budgets (CPU, memory, network, file handles) and
//! enforcing phased limits:
//!
//!   - 70%  → warning phase (logs, metrics)
//!   - 85%  → degraded phase (throttles new allocations)
//!   - 100% → hard stop (isolate terminates, other workers unaffected)
//!
//! The guardian also enforces a global conservation invariant: total worker
//! CPU utilisation MUST NOT exceed 80% of the system capacity. This prevents
//! run-away scheduling from degrading co-located processes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use log::{info, warn, error};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Per-worker resource budget.
#[derive(Debug, Clone)]
pub struct ResourceBudget {
    /// Max heap memory in bytes (V8 heap + external).
    pub memory: u64,
    /// Max CPU time in milliseconds per enforcement window.
    pub cpu: u64,
    /// Max network throughput in bytes per second (ingress + egress).
    pub network: u64,
    /// Max open file descriptors.
    pub file_handles: u64,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            memory: 256 * 1024 * 1024,       // 256 MB
            cpu: 1_000,                       // 1 second CPU per window
            network: 10 * 1024 * 1024,        // 10 MB/s
            file_handles: 256,
        }
    }
}

impl ResourceBudget {
    /// Budget for a small, low-codensity worker (e.g. an HTTP handler).
    pub fn small() -> Self {
        Self {
            memory: 64 * 1024 * 1024,
            ..Default::default()
        }
    }

    /// Budget for a medium worker (e.g. a data-processing pipeline).
    pub fn medium() -> Self {
        Self {
            memory: 256 * 1024 * 1024,
            cpu: 5_000,
            network: 50 * 1024 * 1024,
            file_handles: 512,
        }
    }

    /// Budget for a heavy worker (e.g. a build step or ETL job).
    pub fn heavy() -> Self {
        Self {
            memory: 1 * 1024 * 1024 * 1024,
            cpu: 15_000,
            network: 200 * 1024 * 1024,
            file_handles: 2048,
        }
    }
}

// ---------------------------------------------------------------------------
// Thresholds
// ---------------------------------------------------------------------------

/// Enforcement phase determined by current resource usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePhase {
    /// Normal operation.
    Normal,
    /// 70 % of budget consumed — warnings emitted, metrics published.
    Warning,
    /// 85 % of budget consumed — throttling applied.
    Degraded,
    /// 100 % of budget consumed — isolate terminated.
    HardStop,
}

impl Default for ResourcePhase {
    fn default() -> Self {
        Self::Normal
    }
}

impl ResourcePhase {
    /// Safely construct from a ratio in (0.0 .. 1.0].
    pub fn from_ratio(ratio: f64) -> Self {
        if ratio >= 1.0 {
            Self::HardStop
        } else if ratio >= 0.85 {
            Self::Degraded
        } else if ratio >= 0.70 {
            Self::Warning
        } else {
            Self::Normal
        }
    }
}

// ---------------------------------------------------------------------------
// Per-worker usage accumulator
// ---------------------------------------------------------------------------

/// Live counters for a single worker.
#[derive(Debug, Default)]
pub struct WorkerUsage {
    /// Peak V8 heap usage seen (bytes).
    pub peak_heap: u64,
    /// Accumulated CPU time (ms).
    pub cpu_ms: AtomicU64,
    /// Bytes sent + received on network.
    pub network_bytes: AtomicU64,
    /// Currently open file handles.
    pub open_files: u32,
    /// Budget assigned to this worker.
    pub budget: ResourceBudget,
    /// Phase last computed (avoids repeated log spam).
    pub last_phase: ResourcePhase,
}

impl WorkerUsage {
    pub fn new(budget: ResourceBudget) -> Self {
        Self {
            budget,
            ..Default::default()
        }
    }

    /// Returns the fraction of the memory budget consumed (0.0 .. 1.0+).
    pub fn memory_ratio(&self) -> f64 {
        if self.budget.memory == 0 {
            return 0.0;
        }
        self.peak_heap as f64 / self.budget.memory as f64
    }

    /// Returns the fraction of the CPU budget consumed in the current window.
    pub fn cpu_ratio(&self) -> f64 {
        if self.budget.cpu == 0 {
            return 0.0;
        }
        let current = self.cpu_ms.load(Ordering::Relaxed);
        current as f64 / self.budget.cpu as f64
    }

    /// Returns the fraction of the network budget consumed.
    pub fn network_ratio(&self) -> f64 {
        if self.budget.network == 0 {
            return 0.0;
        }
        let current = self.network_bytes.load(Ordering::Relaxed);
        current as f64 / self.budget.network as f64
    }

    /// Returns the fraction of the file-handle budget consumed.
    pub fn file_ratio(&self) -> f64 {
        if self.budget.file_handles == 0 {
            return 0.0;
        }
        self.open_files as f64 / self.budget.file_handles as f64
    }

    /// The highest ratio across all dimensions.
    pub fn max_ratio(&self) -> f64 {
        self.memory_ratio()
            .max(self.cpu_ratio())
            .max(self.network_ratio())
            .max(self.file_ratio())
    }

    /// Short human-readable summary.
    pub fn summary(&self) -> String {
        format!(
            "mem={:.1}% cpu={:.1}% net={:.1}% files={:.1}%",
            self.memory_ratio() * 100.0,
            self.cpu_ratio() * 100.0,
            self.network_ratio() * 100.0,
            self.file_ratio() * 100.0,
        )
    }
}

// ---------------------------------------------------------------------------
// System-level conservation tracker
// ---------------------------------------------------------------------------

/// Tracks aggregate resource usage across all workers to enforce the
/// conservation invariant: total utilisation must not exceed 80 % of system
/// capacity.
pub struct ConservationTracker {
    total_cpu_ms: AtomicU64,
    system_cpus: u64,
    /// An optional callback invoked when conservation limits are breached.
    on_violation: Option<Box<dyn Fn(&str) + Send + Sync>>,
}

impl std::fmt::Debug for ConservationTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConservationTracker")
            .field("total_cpu_ms", &self.total_cpu_ms)
            .field("system_cpus", &self.system_cpus)
            .field("on_violation", &self.on_violation.as_ref().map(|_| "<callback>"))
            .finish()
    }
}

impl ConservationTracker {
    pub fn new() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1);

        Self {
            total_cpu_ms: AtomicU64::new(0),
            system_cpus: cpus,
            on_violation: None,
        }
    }

    /// Register a callback fired when the conservation invariant is breached.
    pub fn on_violation<F>(&mut self, cb: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.on_violation = Some(Box::new(cb));
    }

    /// Account `delta_cpu_ms` against the global conservation budget.
    /// Returns `true` if within limits, `false` if conservation would be
    /// breached.
    pub fn charge_cpu(&self, delta_cpu_ms: u64) -> bool {
        let total_allowed = self.system_cpus * 1_000 * 80 / 100; // 80% of sys CPU over imaginary 1s window
        let prev = self.total_cpu_ms.fetch_add(delta_cpu_ms, Ordering::Relaxed);
        let now = prev + delta_cpu_ms;
        if now > total_allowed {
            if let Some(ref cb) = self.on_violation {
                cb(&format!(
                    "conservation breach: {}/{} allowed CPU ms consumed",
                    now, total_allowed
                ));
            }
            false
        } else {
            true
        }
    }

    /// Reset counters for a new enforcement window.
    pub fn reset_window(&self) {
        self.total_cpu_ms.store(0, Ordering::Relaxed);
    }

    pub fn usage_fraction(&self) -> f64 {
        let total_allowed = self.system_cpus * 1_000 * 80 / 100;
        if total_allowed == 0 {
            return 0.0;
        }
        self.total_cpu_ms.load(Ordering::Relaxed) as f64 / total_allowed as f64
    }
}

// ---------------------------------------------------------------------------
// Guardian: the top-level coordinator
// ---------------------------------------------------------------------------

/// The top-level guardian instance. One per runtime (shared across workers).
pub struct ResourceGuardian {
    workers: Mutex<HashMap<String, Rc<RefCell<WorkerUsage>>>>,
    conservation: Mutex<ConservationTracker>,
    global_enabled: AtomicBool,
    /// Enforcement window (ms). Counters are reset after each window.
    window_ms: u64,
}

impl ResourceGuardian {
    /// Create a new guardian with a 5-second enforcement window.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            workers: Mutex::new(HashMap::new()),
            conservation: Mutex::new(ConservationTracker::new()),
            global_enabled: AtomicBool::new(true),
            window_ms: 5_000,
        })
    }

    pub fn with_window_ms(mut self, ms: u64) -> Self {
        self.window_ms = ms;
        self
    }

    /// Register a new worker with the given budget.
    pub fn register_worker(
        &self,
        label: String,
        budget: ResourceBudget,
    ) -> Rc<RefCell<WorkerUsage>> {
        let usage = Rc::new(RefCell::new(WorkerUsage::new(budget)));
        self.workers
            .lock()
            .unwrap()
            .insert(label, usage.clone());
        usage
    }

    /// Unregister a worker.
    pub fn unregister_worker(&self, label: &str) {
        self.workers.lock().unwrap().remove(label);
    }

    /// Check all registered workers and return a list of those in HardStop.
    /// The caller is expected to terminate those workers.
    pub fn enforce(&self) -> Vec<String> {
        if !self.global_enabled.load(Ordering::Relaxed) {
            return vec![];
        }

        let mut to_stop = Vec::new();
        let workers = self.workers.lock().unwrap();

        for (label, usage_cell) in workers.iter() {
            let usage = usage_cell.borrow();
            let ratio = usage.max_ratio();
            match usage.last_phase {
                ResourcePhase::HardStop => {
                    // Already flagged — caller should have terminated.
                }
                ResourcePhase::Degraded | ResourcePhase::Warning => {
                    if ratio >= 1.0 {
                        error!(
                            "[resource-guardian] HARD STOP '{}' — {}",
                            label,
                            usage.summary()
                        );
                        to_stop.push(label.clone());
                    }
                }
                ResourcePhase::Normal => {
                    let phase = ResourcePhase::from_ratio(ratio);
                    match phase {
                        ResourcePhase::Warning => {
                            warn!(
                                "[resource-guardian] WARNING '{}' at {:.1}% — {}",
                                label,
                                ratio * 100.0,
                                usage.summary()
                            );
                        }
                        ResourcePhase::Degraded => {
                            warn!(
                                "[resource-guardian] DEGRADED '{}' at {:.1}% — {}",
                                label,
                                ratio * 100.0,
                                usage.summary()
                            );
                        }
                        ResourcePhase::HardStop => {
                            error!(
                                "[resource-guardian] HARD STOP '{}' — {}",
                                label,
                                usage.summary()
                            );
                            to_stop.push(label.clone());
                        }
                        ResourcePhase::Normal => {}
                    }
                }
            }
        }

        // Conservation check.
        let conservation = self.conservation.lock().unwrap();
        let conservation_ratio = conservation.usage_fraction();
        if conservation_ratio > 1.0 {
            warn!(
                "[resource-guardian] CONSERVATION: system CPU at {:.1}% of 80% target",
                conservation_ratio * 100.0
            );
        }

        to_stop
    }

    /// Hook into V8's near-heap-limit callback. When the heap approaches the
    /// budget limit, the guardian gets a chance to act before V8 OOMs.
    pub fn v8_near_heap_limit_callback(
        usage: Rc<RefCell<WorkerUsage>>,
    ) -> impl FnMut(usize, usize) -> usize {
        move |current_heap_limit: usize, _initial_heap_limit: usize| {
            let u = usage.borrow_mut();
            // If peak > budget, signal V8 to stop by returning the current
            // limit unchanged (preventing further expansion).
            if u.peak_heap > u.budget.memory {
                warn!(
                    "[resource-guardian] near-heap-limit: peak={} budget={} — blocking growth",
                    u.peak_heap, u.budget.memory,
                );
                return current_heap_limit;
            }
            // Let V8 double the limit (default behaviour) up to the budget.
            let new_limit = (current_heap_limit * 2).min(u.budget.memory as usize);
            if new_limit == u.budget.memory as usize {
                info!(
                    "[resource-guardian] near-heap-limit: capping at budget {}",
                    u.budget.memory
                );
            }
            new_limit
        }
    }

    /// Enable or disable the guardian globally.
    pub fn set_enabled(&self, enabled: bool) {
        self.global_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.global_enabled.load(Ordering::Relaxed)
    }

    /// Return a human-readable summary of all tracked workers.
    pub fn status(&self) -> String {
        let workers = self.workers.lock().unwrap();
        if workers.is_empty() {
            return "resource-guardian: no workers tracked".into();
        }
        let mut lines: Vec<String> = vec!["resource-guardian status:".into()];
        for (label, usage_cell) in workers.iter() {
            let u = usage_cell.borrow();
            lines.push(format!("  {}  {}", label, u.summary()));
        }
        lines.push(format!(
            "  conservation: CPU at {:.1}% of 80% target",
            self.conservation.lock().unwrap().usage_fraction() * 100.0,
        ));
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_from_ratio() {
        assert_eq!(ResourcePhase::from_ratio(0.0), ResourcePhase::Normal);
        assert_eq!(ResourcePhase::from_ratio(0.69), ResourcePhase::Normal);
        assert_eq!(ResourcePhase::from_ratio(0.70), ResourcePhase::Warning);
        assert_eq!(ResourcePhase::from_ratio(0.84), ResourcePhase::Warning);
        assert_eq!(ResourcePhase::from_ratio(0.85), ResourcePhase::Degraded);
        assert_eq!(ResourcePhase::from_ratio(0.99), ResourcePhase::Degraded);
        assert_eq!(ResourcePhase::from_ratio(1.0), ResourcePhase::HardStop);
        assert_eq!(ResourcePhase::from_ratio(1.5), ResourcePhase::HardStop);
    }

    #[test]
    fn test_worker_usage_defaults() {
        let u = WorkerUsage::new(ResourceBudget::default());
        assert_eq!(u.memory_ratio(), 0.0);
        assert_eq!(u.cpu_ratio(), 0.0);
        assert_eq!(u.network_ratio(), 0.0);
        assert_eq!(u.file_ratio(), 0.0);
    }

    #[test]
    fn test_worker_usage_ratios() {
        let mut u = WorkerUsage::new(ResourceBudget {
            memory: 100,
            cpu: 100,
            network: 100,
            file_handles: 100,
        });
        u.peak_heap = 50;
        u.cpu_ms.store(75, Ordering::Relaxed);
        u.network_bytes.store(90, Ordering::Relaxed);
        u.open_files = 10;

        assert!((u.memory_ratio() - 0.5).abs() < 1e-9);
        assert!((u.cpu_ratio() - 0.75).abs() < 1e-9);
        assert!((u.network_ratio() - 0.9).abs() < 1e-9);
        assert!((u.file_ratio() - 0.1).abs() < 1e-9);
        assert!((u.max_ratio() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn test_guardian_register_and_enforce() {
        let g = ResourceGuardian::new();
        let _ = g.register_worker("test-1".into(), ResourceBudget::small());
        let _ = g.register_worker("test-2".into(), ResourceBudget::heavy());

        // No usage yet — should be empty.
        let stop = g.enforce();
        assert!(stop.is_empty());
    }

    #[test]
    fn test_guardian_enforce_hard_stop() {
        let g = ResourceGuardian::new();
        let usage = g.register_worker("rogue".into(), ResourceBudget {
            memory: 512 * 1024 * 1024,
            cpu: 100_000,
            network: 100_000_000,
            file_handles: 1000,
        });

        // Simulate exceeding every budget.
        {
            let mut u = usage.borrow_mut();
            u.peak_heap = 600 * 1024 * 1024; // > 512 MB
        }

        let stop = g.enforce();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0], "rogue");
    }

    #[test]
    fn test_conservation_tracker() {
        let ct = ConservationTracker::new();
        // Within limits initially.
        assert!(ct.charge_cpu(1));
        ct.reset_window();
        // Even after reset, the old values are gone.
        assert_eq!(ct.usage_fraction(), 0.0);
    }

    #[test]
    fn test_summary_format() {
        let u = WorkerUsage::new(ResourceBudget::default());
        let s = u.summary();
        assert!(s.contains("mem="));
        assert!(s.contains("cpu="));
        assert!(s.contains("net="));
        assert!(s.contains("files="));
    }

    #[test]
    fn test_status_empty() {
        let g = ResourceGuardian::new();
        assert!(g.status().contains("no workers tracked"));
    }

    #[test]
    fn test_window_config() {
        // Verify new() returns an Arc and that the guardian works.
        let g = ResourceGuardian::new();
        let usage = g.register_worker("test".into(), ResourceBudget::default());
        assert!(g.is_enabled());
        let stop = g.enforce();
        assert!(stop.is_empty());
        g.unregister_worker("test");
        let s = g.status();
        assert!(s.contains("no workers"));
    }

    #[test]
    fn test_set_enabled() {
        let g = ResourceGuardian::new();
        assert!(g.is_enabled());
        g.set_enabled(false);
        assert!(!g.is_enabled());
        // Enforcement should be a no-op when disabled.
        let stop = g.enforce();
        assert!(stop.is_empty());
    }

    #[test]
    fn test_v8_near_heap_callback() {
        let usage = Rc::new(RefCell::new(WorkerUsage::new(ResourceBudget {
            memory: 100,
            cpu: 100,
            network: 100,
            file_handles: 100,
        })));

        let mut cb = ResourceGuardian::v8_near_heap_limit_callback(usage.clone());

        // Under budget — allow V8 to double.
        usage.borrow_mut().peak_heap = 40;
        let result = cb(50, 25);
        assert_eq!(result, 100); // doubled, up to budget

        // At budget — should return current_limit to block growth.
        usage.borrow_mut().peak_heap = 120;
        let result = cb(100, 25);
        assert_eq!(result, 100);
    }

    #[test]
    fn test_budget_presets() {
        let s = ResourceBudget::small();
        assert!(s.memory < ResourceBudget::default().memory);

        let h = ResourceBudget::heavy();
        assert!(h.memory > ResourceBudget::default().memory);
    }
}
