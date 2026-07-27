//! Deterministic instruction-count harness for `Python::attach`.
//!
//! Run under callgrind with two different iteration counts and take the
//! difference to get a per-attach instruction count that excludes
//! interpreter startup/shutdown:
//!
//! ```text
//! cargo build --release --example attach_harness
//! valgrind --tool=callgrind ./target/release/examples/attach_harness 10000
//! valgrind --tool=callgrind ./target/release/examples/attach_harness 110000
//! # per-attach cost = (Ir_110000 - Ir_10000) / 100000
//! ```
use pyo3::prelude::*;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    // Initialize the reference pool by dropping a reference on a detached
    // thread, then flush it, so the loop below measures the
    // initialized-but-empty pool state.
    let obj = Python::attach(|py| py.None());
    let reference = Python::attach(|py| obj.clone_ref(py));
    std::thread::spawn(move || drop(reference)).join().unwrap();
    Python::attach(|_| {});

    for _ in 0..n {
        Python::attach(|_| {});
    }
}
