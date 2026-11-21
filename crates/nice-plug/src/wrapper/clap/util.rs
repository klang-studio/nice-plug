use clap_sys::stream::{clap_istream, clap_ostream};
use std::mem::MaybeUninit;
use std::ops::Deref;

/// Early exit out of a function with the specified return value when one of the passed pointers is
/// null.
macro_rules! check_null_ptr {
    ($ret:expr, $ptr:expr $(, $ptrs:expr)* $(, )?) => {
        $crate::wrapper::clap::util::check_null_ptr_msg!("Null pointer passed to function", $ret, $ptr $(, $ptrs)*)
    };
}

/// The same as [`check_null_ptr!`], but with a custom message.
macro_rules! check_null_ptr_msg {
    ($msg:expr, $ret:expr, $ptr:expr $(, $ptrs:expr)* $(, )?) => {
        // Clippy doesn't understand it when we use a unit in our `check_null_ptr!()` macro, even
        // if we explicitly pattern match on that unit
        #[allow(clippy::unused_unit)]
        if $ptr.is_null() $(|| $ptrs.is_null())* {
            crate::nice_debug_assert_failure!($msg);
            return $ret;
        }
    };
}

/// Call a CLAP function. This is needed because even though none of CLAP's functions are allowed to
/// be null pointers, people will still use null pointers for some of the function arguments. This
/// also happens in the official `clap-helpers`. As such, these functions are now `Option<fn(...)>`
/// optional function pointers in `clap-sys`. This macro asserts that the pointer is not null, and
/// prints a nicely formatted error message containing the struct and function name if it is. It
/// also emulates C's syntax for accessing fields struct through a pointer. Except that it uses `=>`
/// instead of `->`. Because that sounds like it would be hilarious.
macro_rules! clap_call {
    { $obj_ptr:expr=>$function_name:ident($($args:expr),* $(, )?) } => {
        match (*$obj_ptr).$function_name {
            Some(function_ptr) => function_ptr($($args),*),
            None => panic!("'{}::{}' is a null pointer, but this is not allowed", $crate::wrapper::clap::util::type_name_of_ptr($obj_ptr), stringify!($function_name)),
        }
    }
}

/// [`clap_call!()`], wrapped in an unsafe block.
macro_rules! unsafe_clap_call {
    { $($args:tt)* } => {
        unsafe { $crate::wrapper::clap::util::clap_call! { $($args)* } }
    }
}

/// Similar to, [`std::any::type_name_of_val()`], but on stable Rust, and stripping away the pointer
/// part.
#[must_use]
pub fn type_name_of_ptr<T: ?Sized>(_ptr: *const T) -> &'static str {
    std::any::type_name::<T>()
}

pub(crate) use check_null_ptr_msg;
pub(crate) use clap_call;

/// Send+Sync wrapper around CLAP host extension pointers.
#[derive(Clone)]
pub struct ClapPtr<T> {
    inner: *const T,
}

impl<T> Deref for ClapPtr<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner }
    }
}

unsafe impl<T> Send for ClapPtr<T> {}
unsafe impl<T> Sync for ClapPtr<T> {}

impl<T> ClapPtr<T> {
    /// Create a wrapper around a CLAP object pointer.
    ///
    /// # Safety
    ///
    /// The pointer must point to a valid object with a lifetime that exceeds this object.
    pub unsafe fn new(ptr: *const T) -> Self {
        Self { inner: ptr }
    }
}

/// A buffer a stream can be read into. This is needed to allow reading into uninitialized vectors
/// using slices without invoking UB.
///
/// # Safety
///
/// This may only be implemented by slices of `u8` and types with the same representation as `u8`.
pub unsafe trait ByteReadBuffer {
    /// The length of the slice, in bytes.
    fn len(&self) -> usize;

    /// Get a pointer to the start of the stream.
    fn as_mut_ptr(&mut self) -> *mut u8;
}

unsafe impl ByteReadBuffer for &mut [u8] {
    fn len(&self) -> usize {
        // Bit of a fun one since we reuse the names of the original functions
        <[u8]>::len(self)
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        <[u8]>::as_mut_ptr(self)
    }
}

unsafe impl ByteReadBuffer for &mut [MaybeUninit<u8>] {
    fn len(&self) -> usize {
        <[MaybeUninit<u8>]>::len(self)
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        <[MaybeUninit<u8>]>::as_mut_ptr(self) as *mut u8
    }
}

/// Fill a slice using the host's potentially short reads.
fn read_stream(stream: &clap_istream, mut slice: impl ByteReadBuffer) -> bool {
    let Some(read) = stream.read else {
        return false;
    };
    let mut offset = 0;
    while offset < slice.len() {
        let remaining = slice.len() - offset;
        // SAFETY: The slice has `remaining` bytes available after `offset`.
        let count = unsafe {
            read(
                stream,
                slice.as_mut_ptr().add(offset).cast(),
                remaining as u64,
            )
        };
        let Ok(count) = usize::try_from(count) else {
            return false;
        };
        if count == 0 || count > remaining {
            return false;
        }
        offset += count;
    }
    true
}

/// Write a slice using the host's potentially short writes.
fn write_stream(stream: &clap_ostream, mut slice: &[u8]) -> bool {
    let Some(write) = stream.write else {
        return false;
    };
    while !slice.is_empty() {
        // SAFETY: The input points to all `slice.len()` bytes for this call.
        let count = unsafe { write(stream, slice.as_ptr().cast(), slice.len() as u64) };
        let Ok(count) = usize::try_from(count) else {
            return false;
        };
        if count == 0 || count > slice.len() {
            return false;
        }
        slice = &slice[count..];
    }
    true
}

/// Read one little-endian length-prefixed CLAP state record.
pub(super) fn read_state_record(
    stream: &clap_istream,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut length = [0_u8; 8];
    if !read_stream(stream, length.as_mut_slice()) {
        return Err("could not read the state length");
    }
    let length = usize::try_from(u64::from_le_bytes(length)).map_err(|_| "invalid state length")?;
    if length > limit {
        return Err("state length exceeds the limit");
    }

    let mut payload = Vec::new();
    payload
        .try_reserve_exact(length)
        .map_err(|_| "could not allocate the state buffer")?;
    if !read_stream(stream, &mut payload.spare_capacity_mut()[..length]) {
        return Err("could not read the complete state payload");
    }
    // SAFETY: The stream initialized all `length` bytes of the reserved capacity above.
    unsafe { payload.set_len(length) };
    Ok(payload)
}

/// Write one little-endian length-prefixed CLAP state record.
pub(super) fn write_state_record(
    stream: &clap_ostream,
    payload: &[u8],
    limit: usize,
) -> Result<(), &'static str> {
    if payload.len() > limit {
        return Err("state length exceeds the limit");
    }
    if !write_stream(stream, &(payload.len() as u64).to_le_bytes()) {
        return Err("could not write the state length");
    }
    if !write_stream(stream, payload) {
        return Err("could not write the complete state payload");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cmp;
    use std::ffi::c_void;
    use std::ptr;

    use super::*;

    struct ReadState<'a> {
        input: &'a [u8],
        max_chunk: u64,
        invalid_result: Option<i64>,
    }

    unsafe extern "C" fn read(stream: *const clap_istream, out: *mut c_void, len: u64) -> i64 {
        // SAFETY: Our test stream holds this live context.
        let context = unsafe { &mut *((*stream).ctx.cast::<ReadState<'_>>()) };
        if let Some(result) = context.invalid_result {
            return result;
        }
        let count = cmp::min(
            cmp::min(len, context.max_chunk) as usize,
            context.input.len(),
        );
        // SAFETY: This host callback writes only the bytes requested by the wrapper.
        unsafe { ptr::copy_nonoverlapping(context.input.as_ptr(), out.cast(), count) };
        context.input = &context.input[count..];
        count as i64
    }

    impl ReadState<'_> {
        fn stream(&mut self) -> clap_istream {
            clap_istream {
                ctx: ptr::from_mut(self).cast(),
                read: Some(read),
            }
        }
    }

    struct WriteState {
        output: Vec<u8>,
        max_chunk: u64,
        failure: Option<(usize, i64)>,
    }

    unsafe extern "C" fn write(stream: *const clap_ostream, input: *const c_void, len: u64) -> i64 {
        // SAFETY: Our test stream holds this live context.
        let context = unsafe { &mut *((*stream).ctx.cast::<WriteState>()) };
        if let Some((after, result)) = context.failure
            && context.output.len() >= after
        {
            return result;
        }
        let count = cmp::min(len, context.max_chunk) as usize;
        // SAFETY: The wrapper flagged these bytes as readable and the test consumes only a prefix.
        let input = unsafe { std::slice::from_raw_parts(input.cast(), count) };
        context.output.extend_from_slice(input);
        count as i64
    }

    impl WriteState {
        fn stream(&mut self) -> clap_ostream {
            clap_ostream {
                ctx: ptr::from_mut(self).cast(),
                write: Some(write),
            }
        }
    }

    #[test]
    fn state_records_allow_short_transfers_and_leave_following_bytes_unread() {
        let payload = br#"{"params":{},"fields":{}}"#;
        let mut writer = WriteState {
            output: Vec::new(),
            max_chunk: 2,
            failure: None,
        };
        write_state_record(&writer.stream(), payload, payload.len()).unwrap();
        assert_eq!(&writer.output[..8], &(payload.len() as u64).to_le_bytes());
        assert_eq!(&writer.output[8..], payload);
        writer.output.extend_from_slice(b"next record");

        let mut reader = ReadState {
            input: &writer.output,
            max_chunk: 3,
            invalid_result: None,
        };
        assert_eq!(
            read_state_record(&reader.stream(), payload.len()),
            Ok(payload.to_vec())
        );
        assert_eq!(reader.input, b"next record");
    }

    #[test]
    fn a_valid_json_prefix_is_not_a_complete_record() {
        let payload = br#"{"params":{},"fields":{}}"#;
        let limit = payload.len() + 1;
        let mut frame = (limit as u64).to_le_bytes().to_vec();
        frame.extend_from_slice(payload);
        let mut reader = ReadState {
            input: &frame,
            max_chunk: 2,
            invalid_result: None,
        };
        assert!(read_state_record(&reader.stream(), limit).is_err());
        let mut reader = ReadState {
            input: &frame[..7],
            max_chunk: 2,
            invalid_result: None,
        };
        assert!(read_state_record(&reader.stream(), limit).is_err());
    }

    #[test]
    fn oversized_records_never_read_or_write_the_body_or_emit_a_header() {
        let mut frame = 2_u64.to_le_bytes().to_vec();
        frame.extend_from_slice(b"ab");
        let mut reader = ReadState {
            input: &frame,
            max_chunk: 2,
            invalid_result: None,
        };
        assert!(read_state_record(&reader.stream(), 1).is_err());
        assert_eq!(reader.input, b"ab");

        let mut writer = WriteState {
            output: Vec::new(),
            max_chunk: 2,
            failure: None,
        };
        assert!(write_state_record(&writer.stream(), b"ab", 1).is_err());
        assert!(writer.output.is_empty());
    }

    #[test]
    fn stream_errors_and_overreported_progress_fail() {
        for result in [-1, 0, 9, i64::MAX] {
            let mut reader = ReadState {
                input: b"",
                max_chunk: 2,
                invalid_result: Some(result),
            };
            assert!(read_state_record(&reader.stream(), 64).is_err());

            let mut writer = WriteState {
                output: Vec::new(),
                max_chunk: 2,
                failure: Some((0, result)),
            };
            assert!(write_state_record(&writer.stream(), b"ab", 64).is_err());
            assert!(writer.output.is_empty());
        }
    }
    #[test]
    fn partial_state_payload_write_reports_failure() {
        let payload = b"abcdef";
        let mut writer = WriteState {
            output: Vec::new(),
            max_chunk: 2,
            failure: Some((10, -1)),
        };
        assert_eq!(
            write_state_record(&writer.stream(), payload, payload.len()),
            Err("could not write the complete state payload"),
        );
        assert_eq!(&writer.output[..8], &(payload.len() as u64).to_le_bytes());
        assert_eq!(&writer.output[8..], b"ab");
    }
}
