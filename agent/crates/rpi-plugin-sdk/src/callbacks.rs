//! SDK-owned C ABI trampolines for string inputs, output slots and deallocation.
//!
//! Raw validity and allocation ownership remain the ABI caller's contract.
//! Plugin bodies use Rust strings and an exclusive, lifetime-bound output
//! capability instead of manipulating pointers themselves.
use crate::{FreeStringFn, StbString, StepResult, StepResultTag};

/// Rust-side ownership of an ABI string and its producer's deallocator.
/// Not `Copy` or `Clone`: moving the value transfers cleanup responsibility.
///
/// ```compile_fail
/// use rpi_plugin_sdk::callbacks::OwnedString;
/// let owner = OwnedString::from_string("text".into());
/// let copy = owner;
/// drop(owner); // Ownership moved; a second release cannot be expressed safely.
/// ```
pub struct OwnedString {
    raw: StbString,
    free: Option<FreeStringFn>,
}

impl OwnedString {
    pub fn from_string(text: String) -> Self {
        Self {
            raw: StbString::from_string(text),
            free: Some(free_string),
        }
    }

    /// Adopt an owning string at an ABI boundary.
    ///
    /// # Safety
    /// The caller transfers unique ownership of a live buffer satisfying
    /// `StbString::to_string_lossy`'s requirements. No raw copy may subsequently
    /// be read, changed or freed while this owner exists. `free`, when present,
    /// must be this buffer's producer-side deallocator, callable from any thread.
    /// When absent, the allocation will not be reclaimed automatically.
    pub unsafe fn from_raw(raw: StbString, free: Option<FreeStringFn>) -> Self {
        Self { raw, free }
    }

    pub fn to_string_lossy(&self) -> String {
        self.as_str_lossy().into_owned()
    }

    /// Read the buffer without copying valid UTF-8. Invalid UTF-8 is replaced,
    /// matching [`Self::to_string_lossy`]. The view cannot outlive its owner.
    ///
    /// ```compile_fail
    /// use rpi_plugin_sdk::callbacks::OwnedString;
    /// let text = {
    ///     let owner = OwnedString::from_string("text".into());
    ///     owner.as_str_lossy()
    /// };
    /// println!("{text}");
    /// ```
    pub fn as_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        if self.raw.len == 0 || self.raw.ptr.is_null() {
            return std::borrow::Cow::Borrowed("");
        }
        // SAFETY: construction established exclusive ownership of a live buffer.
        // This borrow keeps the owner (and its producer-side deallocator) alive.
        let bytes = unsafe { std::slice::from_raw_parts(self.raw.ptr.cast::<u8>(), self.raw.len) };
        String::from_utf8_lossy(bytes)
    }

    /// Transfer ownership back across the ABI, without freeing the buffer.
    /// The receiver must use the same producer's deallocator exactly once.
    pub fn into_raw(self) -> StbString {
        let owner = std::mem::ManuallyDrop::new(self);
        owner.raw
    }
}

impl Drop for OwnedString {
    fn drop(&mut self) {
        // SAFETY: this non-copyable owner transfers its live allocation once.
        unsafe { self.raw.free_with(self.free) };
    }
}

pub struct Output<'a> {
    slot: Option<&'a mut StbString>,
    written: Option<OwnedString>,
}

impl<'a> Output<'a> {
    /// # Safety
    /// A non-null `slot` must be aligned, initialized, writable and exclusively borrowed
    /// for `'a`. The host reclaims a written string through this plugin's
    /// `free_string` callback after the handler returns.
    pub unsafe fn from_raw(slot: *mut StbString) -> Self {
        Self {
            slot: unsafe { slot.as_mut() },
            written: None,
        }
    }

    pub fn is_available(&self) -> bool {
        self.slot.is_some()
    }

    /// Allocate only when a slot exists. Repeated writes reclaim only outputs
    /// this view itself produced, never a value initially supplied by the host.
    pub fn write(&mut self, text: String) -> bool {
        self.replace(Some(text))
    }

    pub fn clear(&mut self) -> bool {
        self.replace(None)
    }

    fn replace(&mut self, text: Option<String>) -> bool {
        let Some(slot) = self.slot.as_deref_mut() else {
            return false;
        };
        let next = text.map(OwnedString::from_string);
        *slot = next
            .as_ref()
            .map(|owner| owner.raw)
            .unwrap_or_else(StbString::empty);
        // Replacing the owner reclaims only a previous SDK output, never an
        // initial host value. Raw slot aliases are inaccessible during this view.
        self.written = next;
        true
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        if let Some(owner) = self.written.take() {
            // The final output belongs to the ABI receiver, even when the
            // handler panics. Replaced outputs were reclaimed during writes.
            let _ = owner.into_raw();
        }
    }
}

/// ABI deallocation callback for strings produced by this SDK instance.
///
/// Install this as `FreeStringFn` in the plugin's registrars. The ABI caller
/// must pass ownership of a live `StbString::from_*` boxed-slice allocation
/// exactly once. Never use it for a borrowed input or another producer's string.
///
/// # Safety
/// Transfer a live SDK-owned boxed-slice allocation exactly once. Neither
/// another raw copy nor a borrowed input may be freed through this callback.
///
/// ```compile_fail
/// use rpi_plugin_sdk::{callbacks, StbString};
/// callbacks::free_string(StbString::from_string("text".into()));
/// ```
pub unsafe extern "C" fn free_string(value: StbString) {
    if value.is_empty() || value.ptr.is_null() {
        return;
    }
    // SAFETY: the ABI caller transfers the original boxed-slice allocation
    // exactly once; construction and this callback use the same allocator.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            value.ptr as *mut u8,
            value.len,
        )));
    }
}

/// ABI cleanup callback for a step result produced by this SDK instance.
/// Its live union arm and string allocation must follow the `StepResult` ABI.
///
/// # Safety
/// The tag must identify the initialized payload, whose live SDK-owned string
/// allocation is transferred exactly once.
pub unsafe extern "C" fn free_step_result(result: StepResult) {
    // SAFETY: the producer initializes the union arm indicated by its tag.
    let text = unsafe {
        match result.tag {
            StepResultTag::Pending => result.pending_payload().progress,
            StepResultTag::Done => result.done_payload().result,
            StepResultTag::Err => result.err_payload().message,
        }
    };
    // SAFETY: the caller transfers the live SDK-produced payload exactly once.
    unsafe { free_string(text) };
}

/// An owned copy of a plugin-produced step result, without ABI pointers.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Pending(String),
    Done(String),
    Err(String),
}

/// # Safety
/// The result must have a valid live union arm and transfer ownership of a
/// string produced by this SDK instance exactly once.
pub unsafe fn take_step_result(result: StepResult) -> Step {
    // SAFETY: the caller transfers an owned result from this SDK instance.
    unsafe { take_step_result_with(result, free_string) }
}

/// Copy and reclaim a result through the producing plugin's deallocator.
///
/// # Safety
/// The tag must select the initialized payload, which must transfer a live
/// owned string exactly once. `free` must be that string's producer-side
/// deallocator, callable from any thread.
pub unsafe fn take_step_result_with(result: StepResult, free: FreeStringFn) -> Step {
    // SAFETY: the producer's tag identifies its initialized payload.
    let raw = unsafe {
        match result.tag {
            StepResultTag::Pending => result.pending_payload().progress,
            StepResultTag::Done => result.done_payload().result,
            StepResultTag::Err => result.err_payload().message,
        }
    };
    // SAFETY: the caller transfers the selected allocation and its producer.
    let owned = unsafe { OwnedString::from_raw(raw, Some(free)) };
    let copy = owned.to_string_lossy();
    match result.tag {
        StepResultTag::Pending => Step::Pending(copy),
        StepResultTag::Done => Step::Done(copy),
        StepResultTag::Err => Step::Err(copy),
    }
}

/// Copy and reclaim a result returned by a plugin's tool callback. Use only
/// with a live, owned result allocated by this SDK instance, not host memory.
/// The invocation requires an explicit unsafe block; the macro does not prove
/// validity or ownership of an arbitrary expression.
///
/// ```compile_fail
/// use rpi_plugin_sdk::{StbString, StepResult};
/// let raw = StepResult::done(StbString::from_string("text".into()));
/// let owned = rpi_plugin_sdk::take_step_result!(raw);
/// ```
#[macro_export]
macro_rules! take_step_result {
    ($result:expr) => {{
        $crate::callbacks::take_step_result($result)
    }};
}

/// Define a command or prompt callback with a `&str` input and `&mut Output`.
/// The four-argument form also supports runtime-action trampolines. A tuple
/// input `(cwd, reason)` supports resource-discovery callbacks. All forms copy
/// inputs inside the ABI boundary and contain handler panics.
#[macro_export]
macro_rules! string_handler {
    ($(#[$attr:meta])* $vis:vis fn $name:ident(($first:ident, $second:ident), $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name(first: $crate::StbStringRef, second: $crate::StbStringRef,
            slot: *mut $crate::StbString, $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: the host keeps both input buffers and the exclusive
                // output slot valid until this ABI invocation returns.
                let first_text = unsafe { first.as_str() }.to_owned();
                let second_text = unsafe { second.as_str() }.to_owned();
                let $first = first_text.as_str();
                let $second = second_text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
    ($(#[$attr:meta])* $vis:vis fn $name:ident($input:ident, $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name(raw: $crate::StbStringRef, slot: *mut $crate::StbString,
            $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: the host keeps its input buffer and exclusive output
                // slot valid until this ABI invocation returns.
                let text = unsafe { raw.as_str() }.to_owned();
                let $input = text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
    ($(#[$attr:meta])* $vis:vis fn $name:ident($action:ident, $input:ident, $out:ident, $data:ident) $body:block) => {
        $(#[$attr])*
        $vis extern "C" fn $name($action: u32, raw: $crate::StbStringRef, slot: *mut $crate::StbString,
            $data: *mut std::ffi::c_void) -> i32 {
            $crate::guard_or(1, || {
                // SAFETY: as in the three-argument ABI trampoline above.
                let text = unsafe { raw.as_str() }.to_owned();
                let $input = text.as_str();
                let mut output = unsafe { $crate::callbacks::Output::from_raw(slot) };
                let $out = &mut output;
                $body
            })
        }
    };
}

/// Define a tool execute callback whose call id is an ordinary borrowed str.
/// The two-argument form gives the body an OwnedString that reclaims parameters
/// through the host's supplied deallocator. The three-argument form exposes
/// raw parameters and their deallocator for callers implementing their own ABI.
#[macro_export]
macro_rules! execute_handler {
    ($(#[$attr:meta])* fn $name:ident($id:ident, $params:ident) $body:block) => {
        $(#[$attr])*
        extern "C" fn $name(
            raw: $crate::StbStringRef,
            params: $crate::StbString,
            free: Option<$crate::FreeStringFn>,
        ) -> $crate::StepHandle {
            $crate::guard_or(std::ptr::null_mut(), || {
                // SAFETY: execute transfers live host parameters and their
                // producer callback. Adopt before decoding the borrowed id so
                // a malformed id cannot leak the owned input.
                let $params = unsafe { $crate::callbacks::OwnedString::from_raw(params, free) };
                // SAFETY: the host keeps its borrowed call id live for this call.
                let text = unsafe { raw.as_str() }.to_owned();
                let $id = text.as_str();
                $body
            })
        }
    };
    ($(#[$attr:meta])* fn $name:ident($id:ident, $params:ident, $free:ident) $body:block) => {
        $(#[$attr])*
        extern "C" fn $name(
            raw: $crate::StbStringRef,
            $params: $crate::StbString,
            $free: Option<$crate::FreeStringFn>,
        ) -> $crate::StepHandle {
            $crate::guard_or(std::ptr::null_mut(), || {
                // SAFETY: host input is borrowed for this ABI call only.
                let text = unsafe { raw.as_str() }.to_owned();
                let $id = text.as_str();
                $body
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StbStringRef;

    thread_local! {
        static OWNER_FREES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    unsafe extern "C" fn counted_free(value: StbString) {
        if !value.is_empty() {
            OWNER_FREES.with(|count| count.set(count.get() + 1));
        }
        // SAFETY: these tests transfer their live SDK allocation exactly once.
        unsafe { free_string(value) };
    }

    execute_handler! { fn refusing_execute(_id, params) {
        assert_eq!(params.to_string_lossy(), "{}");
        std::ptr::null_mut()
    } }

    #[test]
    fn execute_trampoline_reclaims_parameters_on_refusal_and_invalid_id() {
        OWNER_FREES.with(|count| count.set(0));
        assert!(refusing_execute(
            StbStringRef::from_str("call"),
            StbString::from_string("{}".into()),
            Some(counted_free)
        )
        .is_null());
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
        let bytes = [0xff];
        let id = StbStringRef {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        };
        assert!(
            refusing_execute(id, StbString::from_string("{}".into()), Some(counted_free)).is_null()
        );
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn string_views_borrow_valid_utf8_and_keep_lossy_fallback_and_cleanup() {
        OWNER_FREES.with(|count| count.set(0));
        let raw = StbString::from_string("hello 🐈".into());
        let pointer = raw.ptr.cast::<u8>();
        // SAFETY: each fresh allocation is transferred to exactly one owner.
        let owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
        let view = owner.as_str_lossy();
        assert!(matches!(view, std::borrow::Cow::Borrowed(_)));
        assert_eq!(view.as_ptr(), pointer);
        assert_eq!(view, "hello 🐈");
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 0);
        drop(view);
        drop(owner);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);

        let raw = StbString::from_vec(vec![b'a', 0xff, b'b']);
        // SAFETY: invalid UTF-8 is allowed by the adopted buffer's read contract.
        let owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
        assert!(matches!(owner.as_str_lossy(), std::borrow::Cow::Owned(_)));
        assert_eq!(owner.as_str_lossy(), "a\u{fffd}b");
        assert_eq!(owner.to_string_lossy(), "a\u{fffd}b");
        drop(owner);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);

        // SAFETY: null/zero is the ABI's valid empty buffer.
        let empty = unsafe { OwnedString::from_raw(StbString::empty(), Some(counted_free)) };
        assert!(matches!(
            empty.as_str_lossy(),
            std::borrow::Cow::Borrowed("")
        ));
        drop(empty);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn owned_strings_free_once_on_move_and_unwinding() {
        OWNER_FREES.with(|count| count.set(0));
        let raw = StbString::from_string("live".into());
        // SAFETY: this fresh SDK allocation transfers to counted_free once.
        let owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
        assert_eq!(owner.to_string_lossy(), "live");
        let moved = owner;
        assert_eq!(moved.to_string_lossy(), "live");
        drop(moved);
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 1);
        let result = std::panic::catch_unwind(|| {
            let raw = StbString::from_string("unwind".into());
            // SAFETY: another fresh allocation transfers to the same producer.
            let _owner = unsafe { OwnedString::from_raw(raw, Some(counted_free)) };
            panic!("host parsing failed");
        });
        assert!(result.is_err());
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 2);
    }

    #[test]
    fn into_raw_transfers_cleanup_to_the_receiver() {
        let owner = OwnedString::from_string("transferred".into());
        let raw = owner.into_raw();
        // SAFETY: into_raw transferred the live allocation without freeing it.
        let received = unsafe { OwnedString::from_raw(raw, Some(free_string)) };
        assert_eq!(received.to_string_lossy(), "transferred");
        drop(received);
    }

    #[test]
    fn step_result_uses_the_supplied_producer_for_each_variant() {
        OWNER_FREES.with(|count| count.set(0));
        for (raw, expected) in [
            (
                StepResult::pending(StbString::from_string("progress".into())),
                Step::Pending("progress".into()),
            ),
            (
                StepResult::done(StbString::from_string("done".into())),
                Step::Done("done".into()),
            ),
            (
                StepResult::err(StbString::from_string("error".into())),
                Step::Err("error".into()),
            ),
        ] {
            // SAFETY: each tagged result transfers one live SDK allocation.
            assert_eq!(
                unsafe { take_step_result_with(raw, counted_free) },
                expected
            );
        }
        assert_eq!(OWNER_FREES.with(std::cell::Cell::get), 3);
    }

    string_handler! { fn echo(input, output, _data) {
        i32::from(!output.write(input.to_owned()))
    } }
    string_handler! { fn replace_twice(input, output, _data) {
        output.write(input.to_owned());
        output.write("second".into());
        0
    } }
    string_handler! { fn panicking(_input, output, _data) {
        output.write("valid output before panic".into());
        panic!("callback failed");
    } }
    string_handler! { fn action(action, input, output, _data) {
        output.write(format!("{action}:{input}"));
        0
    } }
    string_handler! { fn discover((cwd, reason), output, _data) {
        i32::from(!output.write(format!("{cwd}:{reason}")))
    } }

    #[test]
    fn discovery_copies_both_inputs_and_handles_missing_output() {
        let mut output = StbString::empty();
        let cwd = StbStringRef::from_str("/项目");
        let reason = StbStringRef::from_str("reload");
        assert_eq!(discover(cwd, reason, &mut output, std::ptr::null_mut()), 0);
        // SAFETY: discover wrote a live SDK allocation; it has not been freed.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "/项目:reload"
        );
        assert_eq!(
            discover(cwd, reason, std::ptr::null_mut(), std::ptr::null_mut()),
            1
        );
    }

    #[test]
    fn malformed_second_discovery_input_does_not_write_output() {
        let bytes = [0xff];
        let reason = StbStringRef {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        };
        let mut output = StbString::empty();
        assert_eq!(
            discover(
                StbStringRef::from_str("/tmp"),
                reason,
                &mut output,
                std::ptr::null_mut()
            ),
            1
        );
        assert!(output.is_empty());
    }

    #[test]
    fn missing_output_is_reported_without_a_pointer_write() {
        assert_eq!(
            echo(
                StbStringRef::from_str("text"),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            1
        );
    }

    #[test]
    fn malformed_utf8_is_rejected_before_the_callback_writes_output() {
        let bytes = [0xff, 0xfe];
        let input = StbStringRef {
            ptr: bytes.as_ptr().cast(),
            len: bytes.len(),
        };
        let mut output = StbString::empty();
        assert_eq!(echo(input, &mut output, std::ptr::null_mut()), 1);
        assert!(output.is_empty());
    }

    #[test]
    fn strings_are_copied_and_repeated_output_writes_are_reclaimed() {
        let mut output = StbString::empty();
        assert_eq!(
            echo(
                StbStringRef::from_str("你好"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "你好"
        );
        output = StbString::empty();
        assert_eq!(
            replace_twice(
                StbStringRef::from_str("first"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "second"
        );
    }

    #[test]
    fn panics_leave_a_valid_reclaimable_output() {
        let mut output = StbString::empty();
        assert_eq!(
            panicking(StbStringRef::empty(), &mut output, std::ptr::null_mut()),
            1
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "valid output before panic"
        );
    }

    #[test]
    fn runtime_action_signature_is_preserved() {
        let mut output = StbString::empty();
        assert_eq!(
            action(
                17,
                StbStringRef::from_str("{}"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "17:{}"
        );
    }

    #[test]
    fn writing_does_not_reclaim_an_initial_host_owned_value() {
        let initial = StbString::from_string("host-owned".into());
        let mut output = initial;
        assert_eq!(
            echo(
                StbStringRef::from_str("plugin-owned"),
                &mut output,
                std::ptr::null_mut()
            ),
            0
        );
        // SAFETY: initial is a live SDK allocation, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(initial, Some(free_string)) }.to_string_lossy(),
            "host-owned"
        );
        // SAFETY: the handler produced live SDK output, freed after this assertion.
        assert_eq!(
            unsafe { OwnedString::from_raw(output, Some(free_string)) }.to_string_lossy(),
            "plugin-owned"
        );
    }

    #[test]
    fn every_step_result_variant_can_be_reclaimed() {
        // SAFETY: each initialized payload transfers its fresh SDK allocation.
        unsafe {
            free_step_result(StepResult::pending(StbString::from_string(
                "progress".into(),
            )));
            free_step_result(StepResult::done(StbString::from_string("done".into())));
            free_step_result(StepResult::err(StbString::from_string("error".into())));
            free_string(StbString::empty());
            assert_eq!(
                take_step_result!(StepResult::err(StbString::from_string("cancelled".into()))),
                Step::Err("cancelled".into())
            );
        }
    }
}
