# Case study: pyca/cryptography's PyO3 usage — bigger API-level opportunities

*Analyzed 2026-07-27 against cryptography @ 078d4db (pyo3 0.28, abi3 wheels) and
pyo3 @ this branch. Companion to OPTIMIZATION_OPPORTUNITIES.md.*

Scope note: per project direction, anything that arrives automatically when
cryptography bumps its abi3 floor is **excluded** from the proposals (METH_FASTCALL
at py310, lifetime-preserving `to_str` at py310, native buffer protocol at py311,
vectorcall call paths at py312). Everything below stays wasteful at any floor.

The unifying theme: **the same bytes get touched 2–4× when once would do, and the
same immutable facts get recomputed per call.**

---

## Tier A — data movement (the biggest, most durable wins)

### A1. `PyBytes::new_with` zero-fills memory the closure then overwrites
`src/types/bytes.rs:125` does `write_bytes(buffer, 0, len)` before calling `init`.
cryptography uses `new_with` for every AEAD one-shot (12 sites in aead.rs), every
KDF derive (10 sites), RSA sign, key exchange, Ed25519/Ed448 sign, `os.urandom`-style
rand, XOF digests — each pays a full-output memset that is 100% overwritten.
**Proposal:** an unchecked/`MaybeUninit` variant (`new_with_uninit`) where the
contract is "closure initializes every byte", or track initialization in debug
builds only. Halves output-buffer write traffic for large messages. Works on all
ABI levels. One counterexample to preserve: `kdf.rs:1659` deliberately uses the
zeroing to build an all-zero salt.

### A2. Unknown-length outputs: alloc-max + shrink instead of Vec + copy
`CipherContext.update(N)` does: zeroed `Vec` of N+16 → OpenSSL writes → `PyBytes::new`
copies n bytes (ciphers.rs:156-164). ECDSA sign has a literal TODO (ec.rs:291),
so do RSA decrypt, `encode_extension_value`, and every DER serialization
(`asn1::write_single` → Vec → PyBytes, 100+ sites).
**Proposal:** finish and promote `PyBytesWriter`/`new_with_writer` (already in
0.29-dev) as *the* pattern for serializers, and add an "allocate max, write,
shrink" primitive for the C-write-into-buffer case. Under non-limited API this is
`_PyBytes_Resize` (in-place); under abi3 pre-3.15 the shrink is one exact-size
copy — still strictly better than today (drops the Vec alloc + memset).
Roughly one alloc + one full memcpy per streaming `update()` saved.

### A3. Buffer acquisition: no `bytes` fast path, and a Box per acquire
Every buffer argument in cryptography goes through `CffiBuf` →
`PyBuffer::<u8>::get`, which heap-allocates a `Box<RawBuffer>`, requests
`PyBUF_FULL_RO` (format+shape+strides — the most expensive negotiation), and
parses the format string (`src/buffer.rs:485-498`). The overwhelmingly common
argument is exact `bytes`, where a pointer+len read suffices. This is per
argument, per call — `AESGCM.encrypt` takes 3 buffer args; `Hash.update` in a
loop pays it every iteration.
**Proposal:** a first-class extractor — fast path `PyBytes` (and `PyByteArray`),
fallback to buffer protocol with `PyBUF_CONTIG_RO`, no Box for the 1-D case —
i.e. what `CffiBuf` reimplements, natively. This also addresses the documented
unsoundness cryptography lives with (buf.rs:119-127: `slice::from_raw_parts` on
a possibly-shared buffer, "we're doing an unsound thing and living with it"):
PyO3 can either bless the pattern with an explicitly-named unsafe API or provide
the safe wrapper it wishes existed.

### A4. `PyBackedBuffer`: an owned, `StableDeref` buffer for zero-copy loads
`load_der_x509_certificate` keeps the input `Py<PyBytes>` alive in a `self_cell`
and parses borrowed — zero copy, but only for exact `bytes`. A `PyBackedBytes`-like
type holding an acquired `Py_buffer` would extend zero-copy document loading to
memoryview/mmap inputs, and the `cryptography-keepalive` crate (which exists
solely to keep `PyBackedBytes` alive while borrowing during ASN.1 encoding)
suggests a keepalive/arena helper belongs next to `PyBackedBytes` upstream.

## Tier B — the x509 object graph (recompute-per-access patterns)

### B1. Cached per-instance getters (`#[getter(cached)]`)
Certificate/CRL/CSR/OCSP hand-roll 14 `PyOnceLock<Py<PyAny>>` fields with
getters doing `get_or_try_init(...)?.bind(py).clone()`. Every access after the
first still pays descriptor dispatch → PyO3 trampoline → OnceLock load → incref.
`functools.cached_property` is unavailable (pyclasses have no instance dict).
**Proposal:** a PyO3-owned per-instance attribute slot where, after first
computation, CPython satisfies the attribute without re-entering Rust. Likely
the single highest-leverage API for this codebase (hot path: `cert.extensions`,
`cert.subject`, repeated in TLS handshake-adjacent code); deletes ~150 lines of
boilerplate. Also covers `Sct.timestamp` (2 Python calls + a kwargs dict per
access today) and `Sct.signature_hash_algorithm` (new Python object per access).

### B2. Declarative import cache with borrowed access
`types.rs` holds **182** `LazyPyImport` statics — the hand-rolled cache every
large PyO3 project reinvents. Its `get(py)` clones a `Bound` (incref/decref pair)
on every access because nothing can hand out `&Bound` from a static.
**Proposal:** `pyo3::import!`-style declarative statics whose accessor returns a
borrowed `&Bound<'py, PyAny>` (sound: the cache is write-once and effectively
immortal). Removes a refcount pair from all 78 `is_instance` + 76 `call` sites
and ~570 lines of boilerplate.

### B3. Fallible pre-sized list construction
37+ sites do `PyList::empty` + per-item `append` (FFI call + growth realloc)
because `PyList::new` demands an infallible `ExactSizeIterator` and parse loops
are fallible. **Proposal:** `PyList::try_new(py, len, impl Iterator<Item =
PyResult<...>>)` (and the tuple equivalent). Worth ~30-50% of list-construction
cost on big SANs/CRLs even under limited API (PyList_SetItem beats
append+realloc).

### B4. abi3 datetime support via the capsule
Under abi3, `PyDateTime::new` calls the Python `datetime` constructor and
extraction does **seven** `getattr`+`extract` round-trips per value
(cryptography's common.rs:493-568) — on every `not_valid_before_utc` access and
per revoked cert when iterating CRLs. The `PyDateTime_CAPI` capsule *is*
importable from limited-API builds; PyO3 just doesn't use it there.
**Proposal:** capsule-backed construction/field access under abi3. ~2-4× on
construction, ~5-10× on extraction. (Needs a careful look at capsule ABI
stability guarantees — the capsule struct layout is version-dependent, so this
may need runtime-version gating; flagged as the main design risk.)

### B5. int ↔ bytes without Python method calls
Serial numbers and DSS signature components round-trip through Python
`int.from_bytes` (with a **fresh kwargs dict per call**) / `bit_length` /
`to_bytes` (asn1.rs:46-99, backend/utils.rs:12-39).
**Proposal:** `PyInt::from_be_bytes` / `to_be_bytes` on all builds: native
`PyLong_From/AsNativeBytes` where available (limited API 3.14+, non-limited
3.13+), the current Python-call fallback otherwise — the API is durable, the
fast path arrives progressively with floors.

## Tier C — pyclass mechanics

### C1. Frozen + interior-mutability story
70/86 pyclasses are frozen; the 16 that aren't mostly guard trivial state
(`used: bool` in KDFs, `Option<Hasher>` in Hash, counters in cipher contexts) —
and therefore pay the borrow-flag RMWs on **the hottest streaming methods**
(`Hash.update`, `CipherContext.update_into`). Free-threading makes non-frozen
strictly worse.
**Proposal:** documented interior-mutability field helpers (`PyMutex<T>`-style,
atomic flags) plus an iterator story that doesn't require `&mut self` in
`__next__`, so codebases like this can be 100% frozen. Synergizes with the
borrow-flag findings in the main survey (Tier 3).

### C2. Hash caching and instance interning for small frozen value types
`ObjectIdentifier` is allocated fresh per OID surfaced (one+ per extension) and
`__hash__` re-hashes from scratch per call; `.name` does a Python-dict probe
that re-enters Rust `__hash__`/`__eq__` per lookup. Certificate/Sct `__hash__`
re-hash entire parsed structures per call.
**Proposal:** `#[pyclass(cache_hash)]` (compute once, store in object — sound
for frozen+eq classes; precedent: CPython str) and a sanctioned pattern/API for
interning well-known immutable instances (a static `Py<T>` table for the ~100
known OIDs).

### C3. Type-identity dispatch table
Chains of 5-8 sequential `is_instance` checks per call (`identify_key_type`,
`encode_general_name`, padding dispatch). cryptography already hand-built the
fix once (`cipher_registry.rs`: a `HashMap` keyed on type identity with
precomputed hashes). **Proposal:** a `TypeDispatchMap<T>` helper keyed on type
pointer with one-time registration.

## App-level quick wins spotted (for cryptography itself, no PyO3 change needed)

- `ciphers.rs:185`: `is_instance(XTS)` inside the `update_into` chunk loop —
  hoist to construction (per-call Python isinstance purely for an error message).
- `common.rs:251`: `[("_validate", false)].into_py_dict(py)` rebuilt per
  NameAttribute during name parsing — build once.
- `hashes.rs:36-67`: `MessageDigest::from_name` string lookup per Hash/HMAC
  construction — memoize keyed on algorithm-object identity.
- Missing `py.detach` on `Hash.update`/HMAC/cipher/AEAD paths — multi-hundred-MB
  operations hold the GIL for their full duration (8 detach sites exist, all in
  RSA/EC).
- `hpke.rs:894/1254/1460`: `enc || ct` concatenation allocates a second
  full-message PyBytes — allocate once, encrypt at offset.
- `utils.rs:319`: prehash sign paths allocate a throwaway `PyBytes` digest
  consumed immediately in Rust.
- x509 chain verification (`verify/mod.rs:30-61`) re-enters Python per
  signature check (~20 attr/isinstance ops + 2 PyBytes copies) — keeping the
  loop in Rust is an app refactor with an estimated 2-5× on non-crypto overhead
  for cheap-key chains.

## Suggested experiment order (same methodology as the main survey)

1. **A1 uninit `PyBytes::new_with`** — smallest diff, broadest effect; benchmark
   with an AEAD-shaped workload (alloc + fill N bytes, N ∈ {64, 4K, 1M}).
2. **A3 bytes-fast-path buffer extractor** — prototype in pyo3, benchmark
   Hash.update-shaped loop (acquire per call); validate against CffiBuf's shape.
3. **B3 `PyList::try_new`** — self-contained, easy criterion bench.
4. **B1 cached getters** — biggest design surface; prototype as a proc-macro
   attribute storing `PyOnceLock` slots, measure `cert.extensions`-shaped access.
5. **A2/B4/B5** — coordinate with upstream (PyBytesWriter exists; datetime
   capsule needs an ABI-stability check; int↔bytes is straightforward).
