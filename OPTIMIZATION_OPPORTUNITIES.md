# PyO3 Optimization Opportunities & Iterative Testing Methodology

*Investigation date: 2026-07-27, at commit 2608e54.*

This document records a survey of the PyO3 codebase for significant performance
optimization opportunities, and proposes a repeatable methodology for testing
optimization theories one at a time. Findings are ranked by
(expected impact ÷ risk), favoring small diffs that preserve API compatibility.

---

## 1. Where the time goes

PyO3's hot paths fall into four buckets, all exercised by the existing
`pyo3-benches` suite:

1. **FFI entry (trampolines / `Python::attach`)** — paid by *every*
   `#[pyfunction]` / `#[pymethods]` call.
2. **Argument extraction & dispatch** — macro-generated code plus
   `src/impl_/extract_argument.rs`.
3. **Data conversion** (`FromPyObject` / `IntoPyObject`) — scalars, strings,
   and collections.
4. **`#[pyclass]` runtime** — borrow checking, allocation/freelist, dealloc.

Much of the low-hanging fruit was already picked (vectorcall for Rust-tuple
args, trusted receiver casts in slots per #5930, `intern!`, borrowed tuple
iteration). What remains clusters around **per-call fixed costs** and
**per-element refcount traffic**.

---

## 2. Ranked opportunities

### Tier 1 — small diffs, low risk, high call-frequency

#### 1.1 Atomic gate before the `ReferencePool` mutex ⭐ top pick — **DONE on this branch**

> **Result (cycle 1):** `empty_pool_attach` 101.2 → 85.8 ns (−15.6%, p<0.01);
> callgrind 637.6 → 615.2 instructions/attach (−22.4, the lock/unlock pair).
> `clean_attach` unchanged (p=0.56). Cost: +5.3 instructions on the rare
> work-queued path (measured 926.0 → 931.2 instr/iter on a dirty-path harness).
**Where:** `src/internal/state.rs:208-220` (`ReferencePool::drop_deferred_references`),
called from `AttachGuard::assume()` (`state.rs:161`), i.e. from **every**
trampoline invocation (`src/impl_/trampoline.rs`) and every `Python::attach`.

Once any `Py<T>` has been dropped while detached (common in any threaded app),
the global `POOL: OnceLock<ReferencePool>` is initialized and every subsequent
attach pays `Mutex::lock()` just to observe an empty `Vec`. On free-threaded
builds all threads entering Rust callbacks serialize through this one mutex.

**Change:** add an `AtomicBool`/`AtomicUsize` "pending" flag to `ReferencePool`;
`drop_deferred_references` early-returns on a relaxed load; `register_decref`
sets it. ~15 lines, zero API impact.
**Measure with:** `bench_attach.rs` (after forcing pool init), `bench_call.rs`,
`bench_pyclass.rs`.

#### 1.2 `#[inline]` on small non-generic hot functions
Lifetime-only-generic impls compile into the pyo3 rlib; without (thin-)LTO,
downstream extension crates pay a real function call for 1–5 instruction bodies.
Confirmed missing `#[inline]` on:

- int conversions: `src/conversions/std/num.rs:59,103,130,147,176,193,252`
- floats: `src/types/float.rs:59,130,186`; strings: `src/types/string.rs:180`,
  `src/conversions/std/string.rs:105,131,142,155,166`
- `bool::extract`: `src/types/boolobject.rs:195`
- container accessors: `PyListMethods::{len,get_item}` (`src/types/list.rs:248,285`),
  `PyTupleMethods::{len,get_item,get_borrowed_item_unchecked}`
  (`src/types/tuple.rs:256,287,295,299`), `PyDictMethods::len` (`src/types/dict.rs:267`)
- pycell borrow checker: `BorrowFlag::{increment,decrement}` and the four
  `BorrowChecker` methods (`src/pycell/impl_.rs:60-181`) — the frozen-class
  `EmptySlot` versions are `#[inline]`, the hot mutable-class ones are not.

~30 one-line changes, no risk. Note: benches link pyo3 in-workspace so this is
best demonstrated via a downstream-crate benchmark or by inspecting codegen.

#### 1.3 Interned-pointer keyword-argument matching
**Where:** `src/impl_/extract_argument.rs:649-721` (`handle_kwargs` +
`find_keyword_parameter_*`); descriptions built in
`pyo3-macros-backend/src/params.rs:168-175`.

Every keyword argument on every call does `PyString::to_str()` (an FFI call —
`PyUnicode_AsUTF8AndSize`) then O(n_params) Rust string compares. CPython
itself matches kwnames by interned-pointer equality first. On abi3 <3.10 the
current path even allocates a `PyBackedStr` per kwarg per call.

**Change:** have the macro generate per-function lazily-interned parameter-name
objects; compare `as_ptr()` first, fall back to the current path on miss.
~100–150 lines. Behavior identical.
**Measure with:** `bench_call.rs` extended with kwargs-heavy cases (gap: none
exist today).

#### 1.4 Fast paths for `()` and tuple args in `call`/`call_method`
**Where:** `src/call.rs:71-101`.

- `obj.call1(())` builds an empty tuple + `PyObject_Call`, while `call0()` uses
  `PyObject_CallNoArgs` — make `()::call_positional` use the no-args call.
- The default `call_method_positional` does `getattr(name)` (allocates a bound
  method) + call, for `()` and all `PyTuple` args; `PyObject_VectorcallMethod`
  (already used for Rust-tuple args, `src/types/tuple.rs:669-918`) avoids the
  bound-method allocation.

~40–100 lines, low risk.
**Measure with:** `bench_call.rs` (`call_method_*` benches exist).

#### 1.5 128-bit int extraction: skip `PyNumber_Index` for exact ints
**Where:** `src/conversions/std/num.rs:516` (`int_convert_128!`).

Always round-trips through `nb_index` (FFI call + owned ref) even when the
input is already an exact `int`. Add a `cast::<PyInt>()` fast path first.
~10 lines. **Measure with:** existing `bench_int128.rs`.

### Tier 2 — medium diffs or medium risk, clear wins

#### 2.1 `Vec<T>` extraction fast paths for `list`/`tuple`
**Where:** `src/conversions/std/vec.rs:66-94`.

For all `T` except `u8`, extracting from a `list` or `tuple` uses
`PySequence_Check` + `PyObject_Size` + **allocating a Python iterator** +
per-item `PyIter_Next` (owned ref, incref/decref per element). The crate
already has zero-refcount iteration machinery (`BorrowedTupleIterator`,
`PyList` unchecked access) used elsewhere.

**Change:** probe `cast::<PyTuple>()` → borrowed iteration (immutable, fully
safe); `cast::<PyList>()` → indexed access (owned refs, or borrowed under
`with_critical_section` with per-step length re-check as in
`BoundListIterator::next_unsynchronized`, `src/types/list.rs:502`). This is
also the `#[derive(FromPyObject)]` field path.
Expected ~1.5–3× on `Vec<i64>`/`Vec<String>` from list/tuple. ~40–70 lines.
**Bench gap:** `bench_frompyobject.rs` only covers `Vec<u8>`; add
`vec_int_from_list`/`from_tuple` first.

#### 2.2 Map extraction: drop 2 incref/decref per entry
**Where:** `src/conversions/std/map.rs:118-152` (also `hashbrown.rs`, `indexmap.rs`).

`dict.iter()` clones key and value per entry (`src/types/dict.rs:606-607`);
the refcount-free `BorrowedDictIter` (`dict.rs:824-891`) is exactly what kwargs
handling already uses. Expected ~10–25% on the existing `extract_hashmap`
bench. ~15 lines; needs the same mutation-safety argument as kwargs.

#### 2.3 Lazy `CastError` classinfo
**Where:** `src/instance.rs:1063` / `src/err/cast_error.rs:14-20`.

Failed `cast`/`cast_exact` probes eagerly build `T::type_object(py).into_any()`
(type-object incref + `Bound` construction) even when the caller immediately
discards the error — which happens on the hot fallback paths of
`f64::extract` (`src/types/float.rs:136`), `u8::sequence_extractor`
(`num.rs:262-265`, two discarded errors per `Vec<u8>`-from-list), `bool`,
`HashSet`, and the pre-3.10 int path. Store a `fn(Python) -> Bound<PyAny>`
instead; fixes every probe site at once. ~30 lines.

#### 2.4 Non-allocating `PyErr` for static string messages
**Where:** `src/err/mod.rs:125-137`.

`PyXxxError::new_err("...")` always heap-allocates a boxed closure. Add a
`PyErrStateInner` variant holding `(type fn, Cow<'static, str>)` raised via
`PyErr_SetString`. ~60–100 lines, internal only.
**Measure with:** existing `bench_err.rs`.

#### 2.5 METH_FASTCALL for `**kwargs`-taking functions
**Where:** `pyo3-macros-backend/src/method.rs:528-536`.

Any signature with `**kwargs` compiles as `METH_VARARGS|METH_KEYWORDS`, so
CPython materializes an args tuple even for keyword-less calls. The runtime
already supports fastcall-with-keywords (`extract_arguments_fastcall`, which
builds the kwargs dict only when keywords are present). Keep Varargs only for
the `(*args, **kwargs)` pass-through case (`is_forwarded_args`). ~10 lines +
benches to confirm the kwargs-present path doesn't regress.

#### 2.6 Freelist: `try_lock` fallback
**Where:** `src/impl_/pyclass.rs:968-1020`.

`#[pyclass(freelist = N)]` takes a blocking `Mutex` on every alloc/dealloc; on
free-threaded builds this can contend. A freelist *miss* is always correct, so
`try_lock()` + fall through to `PyType_GenericAlloc`/normal free is a safe
~20-line improvement.

### Tier 3 — larger or needing soundness review

- **Shrink `PyErr` to pointer size** (`src/err/err_state.rs:21-28`): the state
  is ~40 bytes inline (`Once` + `Mutex<Option<ThreadId>>` + `UnsafeCell`), so
  `PyResult<T>` is large and niche-less, and every `PyErr::fetch` pays an
  atomic RMW in `Once::call_once(|| {})`. An atomic tagged-pointer design
  (normalized = raw exception pointer; lazy = boxed fn) removes both.
  ~100–150 lines, concurrency-sensitive.
- **Borrow-flag RMWs on GIL builds** (`src/pycell/impl_.rs:66-97`): non-frozen
  pyclass `&self`/`&mut self` methods pay CAS + `fetch_sub` per call even when
  the GIL guarantees mutual exclusion. A cfg-gated plain load/store version
  needs a careful soundness argument (guards vs. GIL migration).
- **Fewer atomics in pyclass `tp_dealloc`** (`src/pycell/impl_.rs:231-302`):
  two type-object incref/decref pairs + lazy-cell loads per instance
  destruction; raw-pointer slot access would cut 4 shared-object RMWs per
  dealloc (a free-threaded contention point).
- **Merged TLS bookkeeping in attach path** (`src/internal/state.rs:87-99,345-368`):
  the already-attached path does two thread-local reads where one suffices.
- **Vectorcall for `#[pyclass]` `__call__`** (`pyo3-macros-backend/src/pymethod.rs:403-424`):
  callable pyclasses always go through `tp_call` (tuple+dict). Per-instance
  `vectorcallfunc` on CPython ≥3.12 would be a large win but is a 300+ line
  layout/semantics project.

---

## 3. Existing measurement infrastructure

- **Rust microbenches:** `pyo3-benches/` (isolated workspace, MSRV-exempt),
  20 criterion benches via `codspeed-criterion-compat`. Run:
  `nox -s bench -- <filter>` or `cargo bench` inside `pyo3-benches/`.
- **Python-side benches:** `pytests/` with `pytest-benchmark`
  (`nox -s bench` in `pytests/`, disabled by default via `--benchmark-disable`).
- **CI:** `.github/workflows/benches.yml` runs **CodSpeed in simulation mode**
  (deterministic instruction counts) on every PR, on **Python 3.14t
  (free-threaded)**, covering both Rust and Python benches. CodSpeed posts
  per-benchmark deltas vs `main` on the PR.
- Local determinism: valgrind is broadly available, so
  `cargo codspeed build && cargo codspeed run` (or callgrind directly) gives
  stable counts on noisy machines.

Known coverage gaps (add benches *before* optimizing these): kwargs-heavy
calls, `Vec<T≠u8>` from list/tuple, `String` extract, non-bytes
`into_pyobject` collections, module import/init, free-threaded contention
beyond critical-section creation.

---

## 4. Proposed methodology: one theory per cycle

The loop below is designed so every optimization theory produces an
apples-to-apples, reviewable result, resistant to both machine noise and
benchmark-what-you-fixed bias.

**Step 0 — Baseline branch hygiene.** Each theory gets its own branch off
`main` containing exactly two kinds of commits: (a) benchmark additions,
(b) the optimization itself. Never mix theories in one branch.

**Step 1 — Write or identify the benchmark first, and land it separately.**
If the affected path has no bench (see gaps above), add one in `pyo3-benches`
following the existing pattern (register `[[bench]]` in
`pyo3-benches/Cargo.toml`, import from `codspeed_criterion_compat`, batch
~1000 iterations inside `b.iter` with `black_box`). Landing the bench as its
own commit (ideally its own PR) means CodSpeed establishes a `main` baseline
and the optimization PR's delta is trustworthy.

**Step 2 — Predict before measuring.** Write down the mechanism ("removes one
mutex lock per attach") and a rough expected magnitude. A result wildly above
prediction usually means the benchmark is measuring the wrong thing (dead-code
elimination, cache effects); investigate before celebrating.

**Step 3 — Local inner loop (statistical).**
```
cd pyo3-benches
cargo bench --bench <file> -- --save-baseline before      # on main
git switch <theory-branch>
cargo bench --bench <file> -- --baseline before           # criterion prints deltas
```
Criterion's baseline comparison flags changes with confidence intervals.
On a noisy/shared machine, treat <5% deltas as unproven.

**Step 4 — Local verification (deterministic).** For small deltas or noisy
hosts, use instruction counts: `cargo codspeed build && cargo codspeed run`
(valgrind-based, run-to-run stable), or
`valgrind --tool=callgrind` on the criterion binary with `--profile-time`.
Instruction counts confirm *mechanism* (did the lock disappear?) independent
of wall-clock noise. `perf stat`/`perf record` where available closes the loop
on real hardware behavior.

**Step 5 — Check the matrix.** PyO3's fast paths are heavily cfg-gated. Before
declaring victory, verify the change (at minimum: compiles + tests) on the
axes it touches:
- abi3 / limited API (`--features abi3-py310` etc. — many fast paths vanish here);
- free-threaded (CI benches on 3.14t; atomics-related changes especially);
- oldest supported CPython and PyPy if the path is cfg'd on version;
- MSRV (`rust-version = "1.83"`) for the main crate (benches are exempt).
`cargo test` + `nox -s clippy-all` + `nox -s test` per Contributing.md.

**Step 6 — PR = the arbiter.** Open the PR; `benches.yml` runs the full
CodSpeed suite and posts instruction-count deltas vs main for *all* benchmarks
— this catches regressions in paths you didn't think you touched. Include the
prediction, local criterion numbers, and CodSpeed link in the PR description.
Add a `newsfragments/` entry per Contributing.md.

**Step 7 — Guard against regression.** The bench added in Step 1 stays in the
suite permanently, so the win is continuously protected by the per-PR CodSpeed
run.

**Acceptance criteria per theory:**
- CodSpeed shows the predicted improvement on the targeted bench(es);
- no other bench regresses beyond noise;
- no public API/semantics change (or an approved changelog entry if there is);
- soundness-sensitive changes (anything touching `unsafe`, atomics orderings,
  borrow flags) get a written safety argument in the PR.

**Suggested first cycle:** item 1.1 (ReferencePool atomic gate). It is ~15
lines, has zero compatibility surface, is exercised by existing benches
(`bench_attach`, `bench_call`, `bench_pyclass`), and the free-threaded CI
runner will show the scalability effect. Items 1.2 and 1.5 make good
follow-ups while 1.3/2.1 benches are being added.
