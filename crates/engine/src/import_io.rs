//! Bounded independent filesystem requests, preserving input order. Own design.
//! A separate pool avoids occupying render workers with slow network metadata calls.
pub(crate) fn map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync + Send) -> Vec<R> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
        if items.len() > 1
            && let Some(pool) =
                POOL.get_or_init(|| rayon::ThreadPoolBuilder::new().num_threads(8).thread_name(|n| format!("import-io-{n}")).build().ok())
        {
            return pool.install(|| items.par_iter().map(&f).collect());
        }
    }
    items.iter().map(f).collect()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn lightroom_network_requests_overlap_without_reordering_or_omission() {
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let jobs: Vec<_> = (0..32).collect();
        let work = |n: &usize| {
            let running = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(running, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            active.fetch_sub(1, Ordering::SeqCst);
            n * 2
        };
        let start = std::time::Instant::now();
        let serial: Vec<_> = jobs.iter().map(&work).collect();
        let serial_time = start.elapsed();
        peak.store(0, Ordering::SeqCst);
        let start = std::time::Instant::now();
        let parallel = map(&jobs, work);
        let parallel_time = start.elapsed();
        assert_eq!(serial, parallel);
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= 8);
        eprintln!("32 simulated 10 ms metadata calls: serial {serial_time:?}, parallel {parallel_time:?}");
    }
}
