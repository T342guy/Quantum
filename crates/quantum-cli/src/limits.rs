//! Choosing a thread count that fits in the machine's memory.
//!
//! Each worker holds a full model: hash tables, the match index and the
//! secondary-estimation maps. At level 9 that is several hundred megabytes,
//! so running one worker per core can ask for more memory than the machine
//! has. Rather than let that turn into an OOM kill part-way through a long
//! compression, the thread count is trimmed to fit.

use quantum::Config;

/// Fraction of available memory we are willing to use.
const BUDGET_PERCENT: u64 = 60;

pub struct Plan {
    pub threads: usize,
    pub per_thread_bytes: u64,
    /// Set when the thread count was reduced to fit in memory.
    pub reduced_from: Option<usize>,
}

pub fn plan(cfg: &Config, block_size: usize, requested: usize) -> Plan {
    let per_thread = cfg.memory_for_block(block_size) as u64;
    let Some(available) = available_memory() else {
        return Plan { threads: requested, per_thread_bytes: per_thread, reduced_from: None };
    };
    let budget = available * BUDGET_PERCENT / 100;
    let fits = (budget / per_thread.max(1)).max(1) as usize;
    if fits >= requested {
        Plan { threads: requested, per_thread_bytes: per_thread, reduced_from: None }
    } else {
        Plan { threads: fits, per_thread_bytes: per_thread, reduced_from: Some(requested) }
    }
}

/// Memory the OS says is actually available, in bytes.
#[cfg(target_os = "linux")]
fn available_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn available_memory() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_modest_job_keeps_every_thread() {
        let cfg = Config::new(1);
        let plan = plan(&cfg, 1 << 20, 4);
        assert_eq!(plan.threads, 4);
        assert!(plan.reduced_from.is_none());
        assert!(plan.per_thread_bytes > 0);
    }

    #[test]
    fn thread_count_never_drops_below_one() {
        // Level 9 over a huge block asks for far more than any budget.
        let cfg = Config::new(9);
        let plan = plan(&cfg, 1 << 30, 64);
        assert!(plan.threads >= 1);
        assert!(plan.threads <= 64);
    }

    #[test]
    fn memory_estimate_grows_with_level() {
        let small = Config::new(1).memory_for_block(1 << 24);
        let large = Config::new(9).memory_for_block(1 << 24);
        assert!(large > small * 4, "level 9 should want much more memory: {small} vs {large}");
    }
}
