// TODO https://github.com/PyO3/pyo3/issues/5487
#![allow(clippy::undocumented_unsafe_blocks)]

#[cfg(feature = "experimental-inspect")]
use crate::inspect::{type_hint_subscript, PyStaticExpr};
use crate::platform::prelude::*;
#[cfg(not(Py_GIL_DISABLED))]
use crate::types::{PyList, PyListMethods};
use crate::{
    conversion::{FromPyObject, FromPyObjectOwned, FromPyObjectSequence, IntoPyObject},
    exceptions::PyTypeError,
    ffi,
    types::{PyAnyMethods, PySequence, PyString, PyTuple, PyTupleMethods},
    Borrowed, CastError, PyResult, PyTypeInfo,
};
use crate::{Bound, PyAny, PyErr, Python};

impl<'py, T> IntoPyObject<'py> for Vec<T>
where
    T: IntoPyObject<'py>,
{
    type Target = PyAny;
    type Output = Bound<'py, Self::Target>;
    type Error = PyErr;

    #[cfg(feature = "experimental-inspect")]
    const OUTPUT_TYPE: PyStaticExpr = T::SEQUENCE_OUTPUT_TYPE;

    /// Turns [`Vec<u8>`] into [`PyBytes`], all other `T`s will be turned into a [`PyList`]
    ///
    /// [`PyBytes`]: crate::types::PyBytes
    /// [`PyList`]: crate::types::PyList
    #[inline]
    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        T::owned_sequence_into_pyobject(self, py, crate::conversion::private::Token)
    }
}

impl<'a, 'py, T> IntoPyObject<'py> for &'a Vec<T>
where
    &'a T: IntoPyObject<'py>,
{
    type Target = PyAny;
    type Output = Bound<'py, Self::Target>;
    type Error = PyErr;

    #[cfg(feature = "experimental-inspect")]
    const OUTPUT_TYPE: PyStaticExpr = <&[T]>::OUTPUT_TYPE;

    #[inline]
    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        // NB: we could actually not cast to `PyAny`, which would be nice for
        // `&Vec<u8>`, but that'd be inconsistent with the `IntoPyObject` impl
        // above which always returns a `PyAny` for `Vec<T>`.
        self.as_slice().into_pyobject(py).map(Bound::into_any)
    }
}

impl<'py, T> FromPyObject<'_, 'py> for Vec<T>
where
    T: FromPyObjectOwned<'py>,
{
    type Error = PyErr;

    #[cfg(feature = "experimental-inspect")]
    const INPUT_TYPE: PyStaticExpr = type_hint_subscript!(PySequence::TYPE_HINT, T::INPUT_TYPE);

    fn extract(obj: Borrowed<'_, 'py, PyAny>) -> PyResult<Self> {
        if let Some(extractor) = T::sequence_extractor(obj, crate::conversion::private::Token) {
            return Ok(extractor.to_vec());
        }

        if obj.is_instance_of::<PyString>() {
            return Err(PyTypeError::new_err("Can't extract `str` to `Vec`"));
        }

        // Fast paths for the two most common sequence types, avoiding the
        // iterator object allocation and per-item `PyIter_Next` calls of the
        // generic path below. Exact type checks so that subclasses overriding
        // `__iter__` keep the iterator-protocol behavior.
        if obj.is_exact_instance_of::<PyTuple>() {
            // SAFETY: type was just checked
            let tuple = unsafe { obj.cast_unchecked::<PyTuple>() };
            let mut v = Vec::with_capacity(tuple.len());
            // Borrowed items are sound because tuples are immutable, so the
            // `extract` calls cannot invalidate them.
            for item in tuple.iter_borrowed() {
                v.push(item.extract::<T>().map_err(Into::into)?);
            }
            return Ok(v);
        }

        #[cfg(not(Py_GIL_DISABLED))]
        if obj.is_exact_instance_of::<PyList>() {
            // SAFETY: type was just checked
            let list = unsafe { obj.cast_unchecked::<PyList>() };
            let mut v = Vec::with_capacity(list.len());
            let mut index = 0;
            // Re-check the length on every iteration because the `extract`
            // calls can run arbitrary Python code which may mutate the list;
            // this matches the semantics of the iterator protocol used by the
            // generic path.
            while index < list.len() {
                // SAFETY: index is in bounds (checked above), and taking an
                // owned reference keeps the item alive even if the list is
                // mutated during `extract`.
                let item = unsafe { list.get_item_unchecked(index) };
                v.push(item.extract::<T>().map_err(Into::into)?);
                index += 1;
            }
            return Ok(v);
        }

        extract_sequence(obj)
    }
}

fn extract_sequence<'py, T>(obj: Borrowed<'_, 'py, PyAny>) -> PyResult<Vec<T>>
where
    T: FromPyObjectOwned<'py>,
{
    // Types that pass `PySequence_Check` usually implement enough of the sequence protocol
    // to support this function and if not, we will only fail extraction safely.
    if unsafe { ffi::PySequence_Check(obj.as_ptr()) } == 0 {
        return Err(CastError::new(obj, PySequence::type_object(obj.py()).into_any()).into());
    }

    let mut v = Vec::with_capacity(obj.len().unwrap_or(0));
    for item in obj.try_iter()? {
        v.push(item?.extract::<T>().map_err(Into::into)?);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use crate::conversion::IntoPyObject;
    use crate::platform::prelude::*;
    use crate::types::{PyAnyMethods, PyBytes, PyBytesMethods, PyList};
    use crate::Python;

    #[test]
    fn test_vec_from_list_and_tuple() {
        Python::attach(|py| {
            let list = py.eval(c"[1, 2, 3]", None, None).unwrap();
            assert_eq!(list.extract::<Vec<i64>>().unwrap(), [1, 2, 3]);

            let tuple = py.eval(c"(1, 2, 3)", None, None).unwrap();
            assert_eq!(tuple.extract::<Vec<i64>>().unwrap(), [1, 2, 3]);
        });
    }

    #[test]
    fn test_vec_from_sequence_subclass_honors_custom_iter() {
        // Subclasses of list/tuple which override __iter__ must go through
        // the iterator protocol, not the exact-type fast paths.
        Python::attach(|py| {
            py.run(
                c"class WeirdList(list):\n    def __iter__(self): return iter([7, 8])",
                None,
                None,
            )
            .unwrap();
            let obj = py.eval(c"WeirdList([1, 2, 3])", None, None).unwrap();
            assert_eq!(obj.extract::<Vec<i64>>().unwrap(), [7, 8]);

            py.run(
                c"class WeirdTuple(tuple):\n    def __iter__(self): return iter([9])",
                None,
                None,
            )
            .unwrap();
            let obj = py.eval(c"WeirdTuple((1, 2, 3))", None, None).unwrap();
            assert_eq!(obj.extract::<Vec<i64>>().unwrap(), [9]);
        });
    }

    #[test]
    fn test_vec_intopyobject_impl() {
        Python::attach(|py| {
            let bytes: Vec<u8> = b"foobar".to_vec();
            let obj = bytes.clone().into_pyobject(py).unwrap();
            assert!(obj.is_instance_of::<PyBytes>());
            let obj = obj.cast_into::<PyBytes>().unwrap();
            assert_eq!(obj.as_bytes(), &bytes);

            let nums: Vec<u16> = vec![0, 1, 2, 3];
            let obj = nums.into_pyobject(py).unwrap();
            assert!(obj.is_instance_of::<PyList>());
        });
    }

    #[test]
    fn test_vec_reference_intopyobject_impl() {
        Python::attach(|py| {
            let bytes: Vec<u8> = b"foobar".to_vec();
            let obj = (&bytes).into_pyobject(py).unwrap();
            assert!(obj.is_instance_of::<PyBytes>());
            let obj = obj.cast_into::<PyBytes>().unwrap();
            assert_eq!(obj.as_bytes(), &bytes);

            let nums: Vec<u16> = vec![0, 1, 2, 3];
            let obj = (&nums).into_pyobject(py).unwrap();
            assert!(obj.is_instance_of::<PyList>());
        });
    }

    #[test]
    fn test_strings_cannot_be_extracted_to_vec() {
        Python::attach(|py| {
            let v = "London Calling";
            let ob = v.into_pyobject(py).unwrap();

            assert!(ob.extract::<Vec<String>>().is_err());
            assert!(ob.extract::<Vec<char>>().is_err());
        });
    }

    #[test]
    fn test_extract_bytes_to_vec() {
        Python::attach(|py| {
            let v: Vec<u8> = PyBytes::new(py, b"abc").extract().unwrap();
            assert_eq!(v, b"abc");
        });
    }

    #[test]
    fn test_extract_tuple_to_vec() {
        Python::attach(|py| {
            let v: Vec<i32> = py.eval(c"(1, 2)", None, None).unwrap().extract().unwrap();
            assert_eq!(v, [1, 2]);
        });
    }

    #[test]
    fn test_extract_range_to_vec() {
        Python::attach(|py| {
            let v: Vec<i32> = py
                .eval(c"range(1, 5)", None, None)
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(v, [1, 2, 3, 4]);
        });
    }

    #[test]
    fn test_extract_bytearray_to_vec() {
        Python::attach(|py| {
            let v: Vec<u8> = py
                .eval(c"bytearray(b'abc')", None, None)
                .unwrap()
                .extract()
                .unwrap();
            assert_eq!(v, b"abc");
        });
    }
}
