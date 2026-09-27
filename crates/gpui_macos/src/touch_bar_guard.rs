//! Stops one AppKit exception from aborting the process.
//!
//! On a Mac with a Touch Bar, AppKit's `_NSTouchBarFinder` observes the `nextResponder`
//! of every responder in the active window's chain — the window, its views, and whatever
//! transient views AppKit puts there, such as the field editor AppKit creates when a
//! view starts editing. Those observations are retracted later, from
//! `-[_NSTouchBarFinderObservation invalidate]`, inside AppKit's own display-cycle
//! block:
//!
//! ```text
//! -[_NSTouchBarFinderObservation invalidate]
//! ___NSTouchBarFinderSetNeedsUpdateOnMain_block_invoke_2
//! NSDisplayCycleObserverInvoke / NSDisplayCycleFlush
//! ```
//!
//! When the retraction is not the first one — AppKit queues an update per responder-chain
//! change, and two changes (a window closing, focus moving between windows, the active
//! app changing) can leave two blocks holding the same observation — Foundation raises
//! `NSRangeException` from `removeObserver:forKeyPath:context:`. AppKit catches it and
//! calls `-[NSApplication _crashOnException:]`, whose entire purpose is to abort: the
//! process dies with SIGILL, no Rust frame on the stack and nothing for us to catch.
//!
//! Nothing on our side decides whether that happens. Keeping the window alive instead of
//! releasing it, or waiting before the release, does not help — both were shipped and both
//! still aborted — because the abort is AppKit's own bookkeeping, not a use of something
//! we freed. The same abort is reported by apps that neither use GPUI nor tear windows
//! down the way we do, which is why it is treated as an AppKit defect rather than one of
//! ours:
//!
//! * <https://github.com/longbridge/gpui-kit/issues/3192> — GPUI, intermittent, on
//!   switching the frontmost app.
//! * <https://github.com/kodezine/RustyCAN/issues/95> — winit, root-caused to queued
//!   invalidations retracting an observation that was already gone.
//! * <https://github.com/dashpay/dash-evo-tool/issues/820> — mitigated by ordering every
//!   window out before exit, so AppKit retracts while the objects are still alive.
//! * <https://github.com/johnlindquist/kit/issues/1550> — Electron, on the window object.
//! * <https://github.com/emilk/egui/issues/2768> — eframe, on quitting the app.
//! * <https://github.com/feigeCode/navop/issues/268>,
//!   <https://github.com/feigeCode/navop/issues/308> — the reports this module exists for.
//! * <https://github.com/zed-industries/zed/issues/64819> — the same abort, filed with the
//!   backtrace.
//!
//! So [`install`] takes over `-[NSApplication _crashOnException:]` and swallows exactly
//! one exception: an `NSRangeException` whose reason names `_NSTouchBarFinderObservation`.
//! That is the retraction that had nothing left to retract: there is no view to detach
//! from, no observation left to cancel, and no state for the abort to protect — dropping
//! the exception leaves the process in the state it was already in. Every other exception
//! is forwarded to the original implementation, which aborts exactly as AppKit intended.
//!
//! Only the reason string can identify the case: the exception does not carry the call it
//! came from, and the object it names may already be gone.

use std::ffi::{CStr, c_char};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use objc::runtime::{Class, Imp, Method, Object, Sel};
use objc::{msg_send, sel, sel_impl};

/// The observer AppKit's Touch Bar finder is named as in the exception's reason.
const FINDER_OBSERVER: &str = "_NSTouchBarFinderObservation";

/// The exception Foundation raises when a retraction has nothing left to retract.
const ALREADY_REMOVED: &str = "NSRangeException";

/// The method taken over, and the one AppKit's abort lives in.
const CRASH_ON_EXCEPTION: &str = "_crashOnException:";

/// `-[NSApplication _crashOnException:]` as it was before [`install`] replaced it.
///
/// Held as an address because the guard runs from AppKit's call stack with no borrow to
/// hand it, and `OnceLock` needs the value to be `Sync`.
static ORIGINAL: OnceLock<usize> = OnceLock::new();

/// Whether [`install`] already ran.
///
/// Without this, a second call would record the guard itself as the original and forward
/// every other exception straight back into it.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The signature of the replaced method: `v24@0:8@16` — the exception as its only
/// argument.
type CrashOnException =
    unsafe extern "C" fn(this: *mut Object, selector: Sel, exception: *mut Object);

unsafe extern "C" {
    /// Not re-exported by the `objc` crate, which keeps its own declaration private.
    fn method_setImplementation(method: *mut Method, implementation: Imp) -> Imp;
}

/// Takes over `-[NSApplication _crashOnException:]`, once per process.
///
/// Call on the main thread, before the first display cycle; passing `headless` platforms
/// and test processes through this is pointless but harmless.
pub(crate) fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let Some(method) = crash_on_exception_method() else {
        // Not fatal: without a method to replace, AppKit keeps aborting the way it always
        // did, which is what happens on any macOS that does not have it.
        log::debug!(
            "NSApplication does not implement {CRASH_ON_EXCEPTION}; the Touch Bar guard is off"
        );
        return;
    };

    let ours: Imp = unsafe { std::mem::transmute::<CrashOnException, Imp>(crash_on_exception) };
    let original = unsafe { method_setImplementation(method, ours) };
    let _ = ORIGINAL.set(original as usize);
    log::info!(
        "took over NSApplication's {CRASH_ON_EXCEPTION} to keep the Touch Bar finder's own exception from aborting the process"
    );
}

/// AppKit's abort point, with the one exception above let through.
unsafe extern "C" fn crash_on_exception(this: *mut Object, selector: Sel, exception: *mut Object) {
    let (name, reason) = unsafe { exception_name_and_reason(exception) };
    if is_finder_retraction(&name, &reason) {
        // The abort is AppKit's decision to make a bookkeeping error fatal. This one is
        // not ours to make fatal: the retraction it failed on had already happened, so
        // the state AppKit wanted is the state it has.
        log::warn!(
            "ignored an AppKit Touch Bar observer exception that aborts the process: {name}: {reason}"
        );
        return;
    }

    let Some(original) = original_crash_on_exception() else {
        // Should not happen: the method is only replaced when there was one to replace.
        log::error!("{CRASH_ON_EXCEPTION} was replaced without an original to forward to");
        std::process::abort();
    };
    unsafe { original(this, selector, exception) }
}

/// The only exception [`crash_on_exception`] swallows.
///
/// Kept as a separate, total function so that what it accepts can be read and tested
/// without an AppKit exception in flight.
fn is_finder_retraction(name: &str, reason: &str) -> bool {
    name == ALREADY_REMOVED && reason.contains(FINDER_OBSERVER)
}

/// The `-[NSApplication _crashOnException:]` method to replace, if this macOS has it.
fn crash_on_exception_method() -> Option<*mut Method> {
    let class = Class::get("NSApplication")?;
    let selector = Sel::register(CRASH_ON_EXCEPTION);
    let method = class.instance_method(selector)?;
    // `instance_method` hands out a shared reference; `method_setImplementation` takes the
    // method mutably. Replacing an implementation is how the runtime is meant to be
    // extended, and the method is never used concurrently with the replacement.
    Some(method as *const Method as *mut Method)
}

/// The original implementation, ready to be called.
fn original_crash_on_exception() -> Option<CrashOnException> {
    let address = ORIGINAL.get().copied()?;
    if address == 0 {
        return None;
    }
    // Only ever written from `method_setImplementation`'s return value.
    Some(unsafe { std::mem::transmute::<usize, CrashOnException>(address) })
}

/// `exception.name` and `exception.reason`, as Rust strings.
///
/// # Safety
///
/// `exception`, when not null, must be an `NSException`.
unsafe fn exception_name_and_reason(exception: *mut Object) -> (String, String) {
    if exception.is_null() {
        return (String::new(), String::new());
    }
    unsafe {
        let name: *mut Object = msg_send![exception, name];
        let reason: *mut Object = msg_send![exception, reason];
        (nsstring(name), nsstring(reason))
    }
}

/// An `NSString` as a Rust string. Null and non-UTF-8 strings read as empty.
///
/// # Safety
///
/// `string`, when not null, must be an `NSString`.
unsafe fn nsstring(string: *mut Object) -> String {
    if string.is_null() {
        return String::new();
    }
    let utf8: *const c_char = unsafe { msg_send![string, UTF8String] };
    if utf8.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(utf8) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc::class;

    #[test]
    fn only_the_finders_own_retraction_is_swallowed() {
        let from_the_finder = "Cannot remove an observer <_NSTouchBarFinderObservation 0x1234> \
                               for the key path \"nextResponder\" from <NSView 0x5678> because \
                               it is not registered as an observer.";
        assert!(is_finder_retraction(ALREADY_REMOVED, from_the_finder));

        // Same exception, someone else's observation: not ours to swallow.
        assert!(!is_finder_retraction(
            ALREADY_REMOVED,
            "Cannot remove an observer <SomeOtherObservation 0x1234> for the key path \
             \"nextResponder\" because it is not registered as an observer."
        ));
        // A different exception that happens to name the finder: AppKit raised it for a
        // reason, and that reason is not this one.
        assert!(!is_finder_retraction("NSGenericException", from_the_finder));
        assert!(!is_finder_retraction(ALREADY_REMOVED, ""));
    }

    #[test]
    fn a_real_exception_is_read_the_way_appkit_writes_it() {
        let name = unsafe { crate::ns_string(ALREADY_REMOVED) };
        let reason = unsafe {
            crate::ns_string(
                "Cannot remove an observer <_NSTouchBarFinderObservation 0x0> for the key path \
                 \"nextResponder\" because it is not registered as an observer.",
            )
        };
        let exception: *mut Object = unsafe {
            let user_info: *mut Object = std::ptr::null_mut();
            msg_send![class!(NSException), exceptionWithName: name reason: reason userInfo: user_info]
        };

        let (read_name, read_reason) = unsafe { exception_name_and_reason(exception) };
        assert_eq!(read_name, ALREADY_REMOVED);
        assert!(is_finder_retraction(&read_name, &read_reason));

        assert!(!is_finder_retraction(&String::new(), &String::new()));
    }

    #[test]
    fn installing_takes_over_the_method_once() {
        install();

        let original = ORIGINAL.get().copied().expect("the method was taken over");
        assert_ne!(original, 0);

        let installed = crash_on_exception_method().expect("NSApplication has the method");
        let now = unsafe { (*installed).implementation() } as usize;
        assert_ne!(
            now, original,
            "the guard should not have recorded itself as the original"
        );

        // A second install must be a no-op, or the guard would forward to itself.
        install();
        assert_eq!(ORIGINAL.get().copied(), Some(original));
        let still = unsafe { (*installed).implementation() } as usize;
        assert_eq!(still, now);
    }
}
