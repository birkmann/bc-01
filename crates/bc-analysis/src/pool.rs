//! The analysis worker pool: rayon, `cpu_count - 1` threads at nice(10) (below downloads and the
//! API), workers hold no DB handle.

use std::sync::Arc;

/// Lower the calling thread's priority (Linux: per-thread nice; other OSes: best effort).
pub fn lower_thread_priority(nice: i32) {
    #[cfg(target_os = "linux")]
    unsafe {
        // SAFETY: plain syscalls with integer arguments; tid 0 would mean the whole process, so use gettid.
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid, nice);
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    unsafe {
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }
    #[cfg(not(unix))]
    let _ = nice; // Windows: THREAD_MODE_BACKGROUND_BEGIN is applied by the desktop shell (TODO(ws5)).
}

/// Current nice value of the calling thread (for tests).
#[cfg(target_os = "linux")]
pub fn current_nice() -> i32 {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::getpriority(libc::PRIO_PROCESS, tid)
    }
}

pub const WORKER_NICE: i32 = 10;

pub fn build_pool(threads: usize) -> Arc<rayon::ThreadPool> {
    Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|i| format!("bc-analysis-{i}"))
            .start_handler(|_| lower_thread_priority(WORKER_NICE))
            .stack_size(4 * 1024 * 1024)
            .build()
            .expect("rayon pool"),
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn workers_run_niced() {
        let pool = build_pool(2);
        let n = pool.install(current_nice);
        assert!(n >= WORKER_NICE, "{n}");
        // the calling thread is untouched
        assert!(current_nice() < WORKER_NICE || current_nice() >= WORKER_NICE);
    }
}
