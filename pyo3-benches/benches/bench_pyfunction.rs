use std::hint::black_box;

use codspeed_criterion_compat::{criterion_group, criterion_main, Bencher, Criterion};

use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyDict};

#[pyfunction(signature = (a, b=2))]
fn simple(a: i64, b: i64) -> i64 {
    a + b
}

#[pyfunction(signature = (a, b=2, **kwargs))]
fn with_kwargs(a: i64, b: i64, kwargs: Option<Bound<'_, PyDict>>) -> i64 {
    a + b + kwargs.map_or(0, |k| k.len() as i64)
}

fn bench_simple_positional(b: &mut Bencher<'_>) {
    Python::attach(|py| {
        let f = wrap_pyfunction!(simple, py).unwrap();
        b.iter(|| {
            for _ in 0..1000 {
                black_box(&f).call1((1,)).unwrap();
            }
        });
    })
}

fn bench_kwargs_fn_positional(b: &mut Bencher<'_>) {
    Python::attach(|py| {
        let f = wrap_pyfunction!(with_kwargs, py).unwrap();
        b.iter(|| {
            for _ in 0..1000 {
                black_box(&f).call1((1,)).unwrap();
            }
        });
    })
}

fn bench_kwargs_fn_with_kwargs(b: &mut Bencher<'_>) {
    Python::attach(|py| {
        let f = wrap_pyfunction!(with_kwargs, py).unwrap();
        let kwargs = [("c", 3), ("d", 4)].into_py_dict(py).unwrap();
        b.iter(|| {
            for _ in 0..1000 {
                black_box(&f).call((1,), Some(&kwargs)).unwrap();
            }
        });
    })
}

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("simple_positional", bench_simple_positional);
    c.bench_function("kwargs_fn_positional", bench_kwargs_fn_positional);
    c.bench_function("kwargs_fn_with_kwargs", bench_kwargs_fn_with_kwargs);
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
