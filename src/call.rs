// TODO https://github.com/PyO3/pyo3/issues/5487
#![allow(clippy::undocumented_unsafe_blocks)]

//! Defines how Python calls are dispatched, see [`PyCallArgs`].for more information.

use crate::ffi_ptr_ext::FfiPtrExt as _;
use crate::types::{PyAnyMethods as _, PyDict, PyString, PyTuple};
#[cfg(any(PyPy, GraalPy, Py_LIMITED_API))]
use crate::IntoPyObjectExt as _;
use crate::{ffi, Borrowed, Bound, Py, PyAny, PyResult};

pub(crate) mod private {
    use super::*;

    pub trait Sealed {}

    impl Sealed for () {}
    impl Sealed for Bound<'_, PyTuple> {}
    impl Sealed for &'_ Bound<'_, PyTuple> {}
    impl Sealed for Py<PyTuple> {}
    impl Sealed for &'_ Py<PyTuple> {}
    impl Sealed for Borrowed<'_, '_, PyTuple> {}
    pub struct Token;
}

/// This trait marks types that can be used as arguments to Python function
/// calls.
///
/// This trait is currently implemented for Rust tuple (up to a size of 12),
/// [`Bound<'py, PyTuple>`] and [`Py<PyTuple>`]. Custom types that are
/// convertible to `PyTuple` via `IntoPyObject` need to do so before passing it
/// to `call`.
///
/// This trait is not intended to used by downstream crates directly. As such it
/// has no publicly available methods and cannot be implemented outside of
/// `pyo3`. The corresponding public API is available through [`call`]
/// ([`call0`], [`call1`] and friends) on [`PyAnyMethods`].
///
/// # What is `PyCallArgs` used for?
/// `PyCallArgs` is used internally in `pyo3` to dispatch the Python calls in
/// the most optimal way for the current build configuration. Certain types,
/// such as Rust tuples, do allow the usage of a faster calling convention of
/// the Python interpreter (if available). More types that may take advantage
/// from this may be added in the future.
///
/// [`call0`]: crate::types::PyAnyMethods::call0
/// [`call1`]: crate::types::PyAnyMethods::call1
/// [`call`]: crate::types::PyAnyMethods::call
/// [`PyAnyMethods`]: crate::types::PyAnyMethods
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot used as a Python `call` argument",
    note = "`PyCallArgs` is implemented for Rust tuples, `Bound<'py, PyTuple>` and `Py<PyTuple>`",
    note = "if your type is convertible to `PyTuple` via `IntoPyObject`, call `<arg>.into_pyobject(py)` manually",
    note = "if you meant to pass the type as a single argument, wrap it in a 1-tuple, `(<arg>,)`"
)]
pub trait PyCallArgs<'py>: Sized + private::Sealed {
    #[doc(hidden)]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>>;

    #[doc(hidden)]
    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>>;

    #[doc(hidden)]
    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        object
            .getattr(method_name)
            .and_then(|method| method.call1(self))
    }
}

impl<'py> PyCallArgs<'py> for () {
    #[cfg(not(any(PyPy, GraalPy, Py_LIMITED_API)))]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        // One null slot for `PY_VECTORCALL_ARGUMENTS_OFFSET`.
        let mut args: [*mut ffi::PyObject; 1] = [core::ptr::null_mut()];
        unsafe {
            ffi::PyObject_VectorcallDict(
                function.as_ptr(),
                args.as_mut_ptr().add(1),
                const { crate::types::tuple::with_vectorcall_arguments_offset(0) },
                kwargs.as_ptr(),
            )
            .assume_owned_or_err(function.py())
        }
    }

    #[cfg(any(PyPy, GraalPy, Py_LIMITED_API))]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        let args = self.into_pyobject_or_pyerr(function.py())?;
        args.call(function, kwargs, token)
    }

    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        unsafe {
            ffi::compat::PyObject_CallNoArgs(function.as_ptr()).assume_owned_or_err(function.py())
        }
    }

    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        unsafe {
            ffi::compat::PyObject_CallMethodNoArgs(object.as_ptr(), method_name.as_ptr())
                .assume_owned_or_err(object.py())
        }
    }
}

impl<'py> PyCallArgs<'py> for Bound<'py, PyTuple> {
    #[inline]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.as_borrowed().call(function, kwargs, token)
    }

    #[inline]
    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.as_borrowed().call_positional(function, token)
    }

    #[inline]
    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.as_borrowed()
            .call_method_positional(object, method_name, token)
    }
}

impl<'py> PyCallArgs<'py> for &'_ Bound<'py, PyTuple> {
    #[inline]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.as_borrowed().call(function, kwargs, token)
    }

    #[inline]
    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.as_borrowed().call_positional(function, token)
    }

    #[inline]
    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.as_borrowed()
            .call_method_positional(object, method_name, token)
    }
}

impl<'py> PyCallArgs<'py> for Py<PyTuple> {
    #[inline]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.bind_borrowed(function.py())
            .call(function, kwargs, token)
    }

    #[inline]
    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.bind_borrowed(function.py())
            .call_positional(function, token)
    }

    #[inline]
    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.bind_borrowed(object.py())
            .call_method_positional(object, method_name, token)
    }
}

impl<'py> PyCallArgs<'py> for &'_ Py<PyTuple> {
    #[inline]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.bind_borrowed(function.py())
            .call(function, kwargs, token)
    }

    #[inline]
    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.bind_borrowed(function.py())
            .call_positional(function, token)
    }

    #[inline]
    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        token: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.bind_borrowed(object.py())
            .call_method_positional(object, method_name, token)
    }
}

impl<'py> PyCallArgs<'py> for Borrowed<'_, 'py, PyTuple> {
    #[inline]
    fn call(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        kwargs: Borrowed<'_, 'py, PyDict>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        unsafe {
            ffi::PyObject_Call(function.as_ptr(), self.as_ptr(), kwargs.as_ptr())
                .assume_owned_or_err(function.py())
        }
    }

    #[inline]
    fn call_positional(
        self,
        function: Borrowed<'_, 'py, PyAny>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        unsafe {
            ffi::PyObject_Call(function.as_ptr(), self.as_ptr(), core::ptr::null_mut())
                .assume_owned_or_err(function.py())
        }
    }

    #[cfg(all(not(any(PyPy, GraalPy)), any(not(Py_LIMITED_API), Py_3_12)))]
    fn call_method_positional(
        self,
        object: Borrowed<'_, 'py, PyAny>,
        method_name: Borrowed<'_, 'py, PyString>,
        _: private::Token,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = object.py();
        // SAFETY: `self` is a valid tuple
        let len = unsafe { ffi::PyTuple_Size(self.as_ptr()) };

        // For small argument counts, copy the item pointers to the stack and
        // use vectorcall, avoiding the temporary bound method object that the
        // `getattr` fallback creates. The borrowed item references stay valid
        // for the duration of the call because `self` keeps the (immutable)
        // tuple alive.
        const MAX_STACK_ARGS: usize = 8;
        if let Ok(len_usize @ 0..=MAX_STACK_ARGS) = usize::try_from(len) {
            let mut args = [core::ptr::null_mut(); MAX_STACK_ARGS + 1];
            args[0] = object.as_ptr();
            for (i, slot) in args[1..=len_usize].iter_mut().enumerate() {
                // SAFETY: `i` is within bounds of the tuple
                *slot = unsafe { ffi::PyTuple_GetItem(self.as_ptr(), i as ffi::Py_ssize_t) };
            }
            return unsafe {
                ffi::PyObject_VectorcallMethod(
                    method_name.as_ptr(),
                    args.as_mut_ptr(),
                    // +1 for the receiver.
                    crate::types::tuple::with_vectorcall_arguments_offset(1 + len_usize),
                    core::ptr::null_mut(),
                )
                .assume_owned_or_err(py)
            };
        }

        object
            .getattr(method_name)
            .and_then(|method| method.call1(self))
    }
}

#[cfg(test)]
#[cfg(feature = "macros")]
mod tests {
    use crate::{
        pyfunction,
        types::{PyDict, PyTuple},
        Py,
    };

    #[pyfunction(signature = (*args, **kwargs), crate = "crate")]
    fn args_kwargs(
        args: Py<PyTuple>,
        kwargs: Option<Py<PyDict>>,
    ) -> (Py<PyTuple>, Option<Py<PyDict>>) {
        (args, kwargs)
    }

    #[test]
    fn test_call() {
        use crate::{
            types::{IntoPyDict, PyAnyMethods, PyDict, PyTuple},
            wrap_pyfunction, Py, Python,
        };

        Python::attach(|py| {
            let f = wrap_pyfunction!(args_kwargs, py).unwrap();

            let args = PyTuple::new(py, [1, 2, 3]).unwrap();
            let kwargs = &[("foo", 1), ("bar", 2)].into_py_dict(py).unwrap();

            macro_rules! check_call {
                ($args:expr, $kwargs:expr) => {
                    let (a, k): (Py<PyTuple>, Py<PyDict>) = f
                        .call(args.clone(), Some(kwargs))
                        .unwrap()
                        .extract()
                        .unwrap();
                    assert!(a.is(&args));
                    assert!(k.is(kwargs));
                };
            }

            // Bound<'py, PyTuple>
            check_call!(args.clone(), kwargs);

            // &Bound<'py, PyTuple>
            check_call!(&args, kwargs);

            // Py<PyTuple>
            check_call!(args.clone().unbind(), kwargs);

            // &Py<PyTuple>
            check_call!(&args.as_unbound(), kwargs);

            // Borrowed<'_, '_, PyTuple>
            check_call!(args.as_borrowed(), kwargs);
        })
    }

    #[test]
    fn test_call_method_positional() {
        use crate::{
            ffi::c_str,
            types::{PyAnyMethods, PyModule, PyTuple},
            Python,
        };
        use std::vec::Vec;

        Python::attach(|py| {
            let module = PyModule::from_code(
                py,
                c_str!(
                    "class C:\n    def m(self, *args, **kwargs):\n        return (args, kwargs)"
                ),
                c_str!("test.py"),
                c"test_module",
            )
            .unwrap();
            let obj = module.getattr("C").unwrap().call0().unwrap();
            let name = crate::types::PyString::new(py, "m");

            // empty args
            let (a, k): (Vec<i32>, crate::Py<crate::types::PyDict>) =
                obj.call_method1(&name, ()).unwrap().extract().unwrap();
            assert!(a.is_empty());
            assert!(k.bind(py).is_empty().unwrap());

            // small tuple: vectorcall fast path
            let args = PyTuple::new(py, [1, 2, 3]).unwrap();
            let (a, _): (Vec<i32>, Option<crate::Py<crate::types::PyDict>>) =
                obj.call_method1(&name, &args).unwrap().extract().unwrap();
            assert_eq!(a, [1, 2, 3]);

            // large tuple: getattr fallback path
            let args = PyTuple::new(py, 0..20).unwrap();
            let (a, _): (Vec<i32>, Option<crate::Py<crate::types::PyDict>>) =
                obj.call_method1(&name, &args).unwrap().extract().unwrap();
            assert_eq!(a, (0..20).collect::<Vec<i32>>());
        })
    }

    #[test]
    fn test_call_positional() {
        use crate::{
            types::{PyAnyMethods, PyNone, PyTuple},
            wrap_pyfunction, Py, Python,
        };

        Python::attach(|py| {
            let f = wrap_pyfunction!(args_kwargs, py).unwrap();

            let args = PyTuple::new(py, [1, 2, 3]).unwrap();

            macro_rules! check_call {
                ($args:expr, $kwargs:expr) => {
                    let (a, k): (Py<PyTuple>, Py<PyNone>) =
                        f.call1(args.clone()).unwrap().extract().unwrap();
                    assert!(a.is(&args));
                    assert!(k.is_none(py));
                };
            }

            // Bound<'py, PyTuple>
            check_call!(args.clone(), kwargs);

            // &Bound<'py, PyTuple>
            check_call!(&args, kwargs);

            // Py<PyTuple>
            check_call!(args.clone().unbind(), kwargs);

            // &Py<PyTuple>
            check_call!(args.as_unbound(), kwargs);

            // Borrowed<'_, '_, PyTuple>
            check_call!(args.as_borrowed(), kwargs);
        })
    }
}
