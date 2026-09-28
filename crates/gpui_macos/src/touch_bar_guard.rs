//! Keeps AppKit's Touch Bar finder from taking the process down with it — without
//! breaking the removals that are supposed to happen.
//!
//! On a Mac with a Touch Bar, AppKit's `_NSTouchBarFinder` observes the `nextResponder`
//! of the responders in the key window's chain — the window, its views, the field editor
//! AppKit installs while a text view is edited — and retracts those observations later,
//! inside AppKit's own display-cycle block:
//!
//! ```text
//! -[_NSTouchBarFinderObservation invalidate]
//! removeObserver:forKeyPath:context:            raises NSRangeException
//! ___NSTouchBarFinderSetNeedsUpdateOnMain_block_invoke_2
//! NSDisplayCycleObserverInvoke / NSDisplayCycleFlush
//! ```
//!
//! AppKit queues one update per responder-chain change, so two changes close together (a
//! window closing, focus moving between windows, the frontmost app changing) leave two
//! enqueued updates holding the same observation, and the second retraction raises
//! `Cannot remove an observer <_NSTouchBarFinderObservation 0x…> for the key path
//! "nextResponder" from <NSView 0x…> because it is not registered as an observer`.
//!
//! AppKit catches that and calls `-[NSApplication _crashOnException:]`, whose entire
//! purpose is to abort: the process dies with SIGILL, no Rust frame on the stack, nothing
//! for us to intercept. The abort is AppKit's own bookkeeping, not a use of something we
//! freed: keeping a window alive instead of releasing it, and waiting before the release,
//! were both shipped in navop and both still aborted, and the same abort reaches apps
//! with none of our teardown (winit's RustyCAN #95, fixed by no-oping the finder's KVO on
//! the window's view; GPUI when the frontmost app changes, gpui-kit #3192; Electron;
//! eframe; and zed#64819, which is the same backtrace this module exists for,
//! navop#268 / navop#308).
//!
//! The first version of this guard answered that exception by skipping *every* retraction
//! the finder makes — `if is_finder_observer(observer) { return; }`. That was too much:
//! a legal registration has to be removable for KVO's contract to hold, and the skip hit
//! the first, legal removal as well as the duplicate. A local experiment proved it: with
//! the guard on, a removal that should have ended the notifications didn't, and the
//! observation stayed registered. It is also the prime suspect for the one crash that
//! came from this module rather than from the exception — `KERN_INVALID_ADDRESS` inside
//! Foundation's KVO bookkeeping during `-[NSApplication terminate:]`
//! (`navop-2026-09-27-165228.ips`): skipping a retraction leaves the finder's observation
//! registered on objects the finder has already dropped its side of.
//!
//! So the guard now lets every retraction *through*, and catches only the one exception
//! it exists for. [`forward`] calls the implementation it replaced inside
//! `objc2::exception::catch`; when that returns, the retraction really happened and
//! Foundation's bookkeeping stays consistent. When it raises, the exception is matched
//! against the one failure this module knows — observer is the finder's, key path is
//! `nextResponder`, the exception is `NSRangeException` and its reason contains
//! `because it is not registered as an observer` — and only that combination is
//! swallowed, with the full retraction logged where it happens: observer class and
//! address, observed object class and address, key path, context, exception name and
//! reason. That log line is the evidence the field reports never captured (their
//! backtraces stop at the raise without name, reason, object or key path), and it is
//! what decides the next step: a duplicate retraction is AppKit's to fix and ours to
//! absorb; anything else — an object freed early, a registration that never matched —
//! gets the exception thrown straight back, because Cocoa is not exception-safe and a
//! broader net would hide bugs, not fix them.
//!
//! The catch is `objc2::exception`'s (`@try`/`@catch` compiled from C, the
//! `exception` Cargo feature), not Rust: Rust cannot catch a foreign exception, and
//! letting one unwind into a Rust frame aborts — which is the very outcome this module
//! prevents. What is caught is rethrown with [`objc2::exception::throw`], unwinding out
//! of this `C-unwind` replacement exactly as it would unwind out of the implementation
//! it replaced.
//!
//! Swallowing the exception one level up, in `-[NSApplication _crashOnException:]`, was
//! tried and is not enough: by the time that method runs the exception has already
//! unwound out of `NSDisplayCycleFlush`, and returning from it leaves the display cycle
//! unfinished, which freezes the app instead of aborting it.
//!
//! Every class gets a replacement of its own, and each forwards to the implementation
//! *that class* had before. One replacement shared by all of them cannot do that: it
//! would have to work out which implementation to call from the receiver, and the
//! receiver does not say. AppKit's `-[NSWindow removeObserver:forKeyPath:context:]`
//! implements the method and hands the retraction on to `NSObject`'s implementation —
//! which the guard has replaced too. A shared replacement entered from the window
//! therefore looks up the receiver's class, finds the window's original, calls it, and
//! is called back by it, for as long as the stack lasts. A Navop build carrying the
//! first version of this module died exactly that way: 512 frames of
//! `-[NSWindow removeObserver:forKeyPath:context:]` alternating with the guard's
//! implementation, then `KERN_PROTECTION_FAILURE` on the stack guard page and `abort()`
//! (`navop-2026-09-28-083919.ips`). The replacement is per class, the slot it forwards
//! to is per class, and the forward target cannot be ambiguous.
//!
//! The guard installs itself unless `GPUI_MACOS_TOUCHBAR_GUARD` says otherwise (`0`,
//! `false`, `off` or `no`). It is on by default because the exception it prevents is the
//! one the field keeps hitting. Replacing methods of AppKit's own classes is still not a
//! change that can be called harmless, so the switch stays until the rethrowing guard
//! has survived a Touch Bar machine in the field; once it has, `window_teardown`'s
//! retirement of closed windows — which this guard makes unnecessary, but which is not
//! removed until the guard is proven — can be undone, and native windows released again.
//!
//! The observed object is AppKit's own `NSView`, `NSWindow` or field editor, so there is
//! no class of ours to override: the replacements go on the classes [`observed_classes`]
//! reports as responders, scoped by the observer's class name — the only handle on a
//! private class. Nothing but the finder's own observations is affected. A class that
//! inherits the method is left alone: an ancestor of it is patched, and rewriting an
//! inherited method would replace the same implementation twice. `NSWindow` is the one
//! that implements the method itself, which is why a single override on `NSObject` is
//! not enough.

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Once, OnceLock};

use objc::runtime::{Class, Imp, Method, Object, Sel};
use objc::{msg_send, sel, sel_impl};

/// The class name AppKit's Touch Bar finder registers its observations under.
///
/// Matched as a substring, so a renamed or companion class (`_NSTouchBarFinderObservation`
/// and friends) is still caught.
const FINDER_OBSERVER: &str = "_NSTouchBarFinder";

/// The key path the finder observes and retracts on.
///
/// The finder registers for nothing else, so a retraction of the finder's on any other
/// key path is not the failure this module knows about and is thrown back.
const FINDER_KEY_PATH: &str = "nextResponder";

/// The exception the duplicate retraction raises.
const RANGE_EXCEPTION: &str = "NSRangeException";

/// What the duplicate retraction's reason says, verbatim from the field reports.
const UNREGISTERED_MARKER: &str = "because it is not registered as an observer";

/// Set to `0`, `false`, `off` or `no` to keep the guard out; it installs itself otherwise.
const GUARD_ENV: &str = "GPUI_MACOS_TOUCHBAR_GUARD";

/// The number of classes the guard can replace the method on at once.
///
/// One replacement each: a class's replacement has to forward to *that* class's
/// implementation, so two classes can never share one. AppKit implements the method on
/// `NSObject` and on `NSWindow`; the rest of the room is for the responders it may grow,
/// and for whatever else declares the method before the guard runs.
const SLOTS: usize = 8;

/// The method on the throwing path that is replaced.
///
/// The finder calls this one; it forwards to `removeObserver:forKeyPath:`, which forwards
/// to the private method that raises. Replacing the outermost of the three is enough.
const REMOVE_OBSERVER: &str = "removeObserver:forKeyPath:context:";

/// The classes whose `nextResponder` the finder observes, and the ones they inherit the
/// method from.
///
/// The finder registers its observation for the key path `nextResponder`, so the object it
/// later retracts on is always a responder: the window, a view, the field editor AppKit
/// installs while a text view is being edited. Every one of those is an `NSResponder`
/// subclass — mostly AppKit's own private ones — or a class that supplies the method they
/// inherit, which in practice means `NSObject`.
///
/// Listing the classes by hand does not work: `NSWindow` implements
/// `removeObserver:forKeyPath:context:` itself, so patching `NSObject` leaves every window
/// retracting straight into the abort, and the rest of AppKit's responder classes are
/// private and free to change. The runtime is asked instead.
fn observed_classes() -> Vec<*const Class> {
    let mut classes = Vec::new();
    if let Some(object) = Class::get("NSObject") {
        classes.push(object as *const Class);
    }
    let Some(responder) = Class::get("NSResponder") else {
        return classes;
    };
    classes.extend(
        loaded_classes()
            .into_iter()
            .filter(|class| is_subclass_of(*class, responder)),
    );
    classes
}

/// Every class registered with the Objective-C runtime.
fn loaded_classes() -> Vec<*const Class> {
    let count = unsafe { objc_getClassList(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    let mut buffer = vec![std::ptr::null(); count as usize];
    let written = unsafe { objc_getClassList(buffer.as_mut_ptr(), count) };
    if written <= 0 {
        return Vec::new();
    }
    // Classes registered between the two calls are not included, which cannot happen for
    // AppKit's responders: they are all there before the first window.
    buffer.truncate(written.min(count) as usize);
    buffer
}

/// Whether `class` inherits from `ancestor`, which is not a subclass of itself.
fn is_subclass_of(class: *const Class, ancestor: *const Class) -> bool {
    let mut class = unsafe { class_getSuperclass(class) };
    while !class.is_null() {
        if std::ptr::eq(class, ancestor) {
            return true;
        }
        class = unsafe { class_getSuperclass(class) };
    }
    false
}

/// Whether [`install`] already ran.
///
/// `Once`, not a flag: a second caller has to wait for the first one to finish, otherwise
/// it could read [`REPLACED`] before it is written — and installing twice would record the
/// guard itself as what it replaced, and forward every other retraction back into it.
static INSTALL: Once = Once::new();

/// Every method this module replaced: its class, and the replacement it was given.
///
/// Addresses rather than pointers, because a static has to be `Sync` and the guard is
/// entered from AppKit's own call stack with no borrow to hand it. The tests read it to
/// check what the runtime actually holds.
static REPLACED: OnceLock<Vec<(usize, usize)>> = OnceLock::new();

/// The implementation behind every slot: the one the class had before the guard took it.
///
/// Zero until [`replace`] has written it, which no replaced method can observe: the
/// original is stored before the replacement goes in.
static ORIGINALS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

/// The next slot to hand out, and how many classes have been replaced.
static SLOTS_TAKEN: AtomicUsize = AtomicUsize::new(0);

/// The signature of the replaced method: `v32@0:8@16@24^v32` — the observer, the key path
/// and the context.
///
/// `C-unwind` rather than `C`: the implementation this one forwards to raises for every
/// retraction that does not match the one failure this module absorbs, and that exception
/// has to reach the caller — AppKit's own display cycle catches it. With the plain C ABI
/// Rust aborts instead of letting it through.
type RemoveObserverForKeyPathContext =
    unsafe extern "C-unwind" fn(*mut Object, Sel, *mut Object, *mut Object, *mut c_void);

unsafe extern "C" {
    /// Not re-exported by the `objc` crate, which keeps its own declaration private.
    fn method_setImplementation(method: *mut Method, implementation: Imp) -> Imp;
    /// Not re-exported by the `objc` crate either.
    fn method_getImplementation(method: *mut Method) -> Imp;
    /// Not re-exported by the `objc` crate either.
    fn object_getClassName(object: *mut Object) -> *const c_char;
    /// Not re-exported by the `objc` crate either.
    fn class_getInstanceMethod(class: *const Class, selector: Sel) -> *mut Method;
    /// Not re-exported by the `objc` crate either.
    fn class_getSuperclass(class: *const Class) -> *const Class;
    /// Not re-exported by the `objc` crate either.
    fn objc_getClassList(buffer: *mut *const Class, buffer_count: i32) -> i32;
}

/// Replaces `removeObserver:forKeyPath:context:` on [`observed_classes`], once per process.
///
/// Installs itself unless [`GUARD_ENV`] turns it off; see the module documentation for why.
///
/// Call on the main thread, before the first window exists.
pub(crate) fn install() {
    if guard_requested() {
        INSTALL.call_once(install_once);
    }
}

/// Whether the guard is on, which is the default.
///
/// An unset variable and an empty one both mean "on": an empty value comes from a wrapper
/// that meant to pass nothing, and silently losing the guard that way is exactly the failure
/// this default exists to avoid.
fn guard_requested() -> bool {
    std::env::var(GUARD_ENV).map_or(true, |value| !guard_disabled_by(&value))
}

/// Whether one value of [`GUARD_ENV`] turns the guard off.
///
/// Total, and separated from the environment so that what counts as "off" can be read and
/// tested without setting anything.
fn guard_disabled_by(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("0")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no")
}

/// The body of [`install`], run once per process.
fn install_once() {
    let mut replaced = Vec::new();
    for class in observed_classes() {
        // `None` means the class does not implement the method itself, so an ancestor of it
        // does — and is patched on its behalf, because it is in the set too.
        if let Some(entry) = replace(class) {
            replaced.push(entry);
        }
    }

    if replaced.is_empty() {
        // Not fatal: without a method to replace, AppKit keeps aborting the way it always
        // did, which is what happens on any Foundation that implements it elsewhere.
        log::error!("no class implements {REMOVE_OBSERVER}; the Touch Bar guard is off");
        return;
    }
    let count = replaced.len();
    let _ = REPLACED.set(replaced);
    log::info!(
        "took over {REMOVE_OBSERVER} on {count} classes; the Touch Bar finder's retractions now run, and only the unregistered-observation exception is absorbed"
    );
}

/// Replaces the method on one class with a replacement of its own, returning the class and
/// the replacement it was given.
///
/// `None` when a class upstream already implements it, which is the case for every class
/// that inherits the method, and when every slot is taken.
fn replace(class: *const Class) -> Option<(usize, usize)> {
    let selector = Sel::register(REMOVE_OBSERVER);
    let method = unsafe { class_getInstanceMethod(class, selector) };
    if method.is_null() {
        return None;
    }

    // `class_getInstanceMethod` resolves inheritance, so a class that does not implement
    // the method itself hands back the method object of the class that does. Replacing that
    // would replace the same implementation twice, and the second time the "original"
    // would be the guard.
    let inherited = unsafe {
        let superclass = class_getSuperclass(class);
        !superclass.is_null() && class_getInstanceMethod(superclass, selector) == method
    };
    if inherited {
        return None;
    }

    let slot = SLOTS_TAKEN.fetch_add(1, Ordering::AcqRel);
    let Some(ours) = forwarder(slot) else {
        let name = unsafe { (*class).name() };
        log::error!(
            "more classes implement {REMOVE_OBSERVER} than the guard has replacements for; {name} keeps Foundation's"
        );
        return None;
    };

    // The original is read before the replacement goes in, so a retraction that arrives
    // while the guard is installing finds the original rather than nothing.
    let original = unsafe { method_getImplementation(method) } as usize;
    ORIGINALS[slot].store(original, Ordering::Release);
    unsafe {
        // `method_setImplementation` takes the method mutably while the runtime hands out
        // shared references to it. Replacing an implementation is how the runtime is meant
        // to be extended, and the method is never used while it is being replaced.
        method_setImplementation(method, std::mem::transmute::<usize, Imp>(ours))
    };
    Some((class as usize, ours))
}

/// One replacement per slot, each forwarding to its own slot's original.
///
/// A macro because the runtime needs a distinct function per class: see the module
/// documentation for what a shared one does. [`forwarder`] and [`ORIGINALS`] share the slot
/// numbering, so the replacement a slot is given always forwards to that slot's original.
macro_rules! forwarders {
    ($($slot:literal => $name:ident),+ $(,)?) => {
        $(
            unsafe extern "C-unwind" fn $name(
                this: *mut Object,
                selector: Sel,
                observer: *mut Object,
                key_path: *mut Object,
                context: *mut c_void,
            ) {
                unsafe { forward($slot, this, selector, observer, key_path, context) }
            }
        )+

        /// The replacement for one slot, ready to hand to the runtime.
        ///
        /// A function rather than a table because a function item cannot be turned into
        /// its address while the compiler is still evaluating a `static`.
        fn forwarder(slot: usize) -> Option<usize> {
            match slot {
                $($slot => Some($name as *const () as usize),)+
                _ => None,
            }
        }
    };
}

forwarders! {
    0 => forward_0,
    1 => forward_1,
    2 => forward_2,
    3 => forward_3,
    4 => forward_4,
    5 => forward_5,
    6 => forward_6,
    7 => forward_7,
}

/// What every replacement does: let every retraction run, and absorb only the finder's
/// unregistered-observation exception.
///
/// A retraction that is not the finder's goes straight to the implementation the slot was
/// taken from, exception included — that behaviour is Foundation's, and the tests hold the
/// guard to it. The finder's own retraction is the one AppKit aborts on, so that one runs
/// inside [`objc2::exception::catch`]: when it returns, the observation is really gone and
/// Foundation's bookkeeping stays consistent; when it raises the one exception this module
/// knows about, the retraction is a duplicate retracting an observation that is already
/// gone, it is logged in full and absorbed; anything else is thrown back, because an
/// unexpected exception there is a bug to surface, not one to hide.
unsafe fn forward(
    slot: usize,
    this: *mut Object,
    selector: Sel,
    observer: *mut Object,
    key_path: *mut Object,
    context: *mut c_void,
) {
    let original = ORIGINALS[slot].load(Ordering::Acquire);
    if original == 0 {
        // Should not happen: a replacement goes in only after its slot is written.
        log::error!("{REMOVE_OBSERVER} was replaced without an original to forward to");
        return;
    }

    if !is_finder_observer(observer) {
        // Only ever written from a method's own implementation, above.
        let original: RemoveObserverForKeyPathContext = unsafe { std::mem::transmute(original) };
        unsafe { original(this, selector, observer, key_path, context) };
        return;
    }

    let key_path_name = key_path_name(key_path);
    let caught = objc2::exception::catch(|| {
        // Only ever written from a method's own implementation, above. Raw pointers,
        // so the closure is `UnwindSafe`, which `catch` requires.
        let original: RemoveObserverForKeyPathContext = unsafe { std::mem::transmute(original) };
        unsafe { original(this, selector, observer, key_path, context) }
    });
    match caught {
        Ok(()) => {
            log::debug!(
                "the Touch Bar finder's retraction of {key_path_name} ran on {} {this:p}",
                class_name(this).as_deref().unwrap_or("<unknown>")
            );
        }
        Err(None) => {
            // A nil exception cannot be rethrown; `@throw nil` is not a thing this path
            // produces. Recorded, not absorbed silently.
            log::error!(
                "the finder's retraction of {key_path_name} raised without an exception object"
            );
        }
        Err(Some(exception)) => {
            if is_unregistered_retraction(&exception, &key_path_name) {
                // The retraction had nothing left to retract, which is AppKit's own
                // double-retraction; doing nothing here is what it meant to do. The log
                // line is the evidence the field reports never carried.
                log::warn!(
                    "absorbed the Touch Bar finder's duplicate retraction of {key_path_name} \
                     from {} {this:p} (observer {} {observer:p}, context {context:p}): \
                     {}: {}",
                    class_name(this).as_deref().unwrap_or("<unknown>"),
                    class_name(observer).as_deref().unwrap_or("<unknown>"),
                    exception_name(&exception).as_deref().unwrap_or("<no name>"),
                    exception_reason(&exception)
                        .as_deref()
                        .unwrap_or("<no reason>"),
                );
            } else {
                // Not the failure this module exists for: let it unwind exactly as the
                // implementation this one replaced would have.
                objc2::exception::throw(exception);
            }
        }
    }
}

/// Whether an exception is the one failure this module absorbs: the finder's observer,
/// the finder's key path, `NSRangeException`, and the reason the field reports carry.
///
/// Anything else — a different key path, a different exception, an object that does not
/// even answer `name` — is not ours to swallow.
fn is_unregistered_retraction(exception: &objc2::exception::Exception, key_path: &str) -> bool {
    if key_path != FINDER_KEY_PATH {
        return false;
    }
    if exception_name(exception).as_deref() != Some(RANGE_EXCEPTION) {
        return false;
    }
    exception_reason(exception).is_some_and(|reason| reason.contains(UNREGISTERED_MARKER))
}

/// An exception's `name`, if it is an `NSException` at all.
///
/// `None` for an exception object that does not answer `name` — thrown objects are not
/// required to be `NSException`s, and one that is not is not ours to inspect.
fn exception_name(exception: &objc2::exception::Exception) -> Option<String> {
    let responds: bool =
        unsafe { objc2::msg_send![exception, respondsToSelector: objc2::sel!(name)] };
    if !responds {
        return None;
    }
    // `name` returns an `NSString`, not owned by us.
    let value: Option<objc2::rc::Retained<objc2::runtime::NSObject>> =
        unsafe { objc2::msg_send![exception, name] };
    string_from_nsobject(value)
}

/// An exception's `reason`, if it is an `NSException` at all.
fn exception_reason(exception: &objc2::exception::Exception) -> Option<String> {
    let responds: bool =
        unsafe { objc2::msg_send![exception, respondsToSelector: objc2::sel!(reason)] };
    if !responds {
        return None;
    }
    // `reason` returns an `NSString`, not owned by us.
    let value: Option<objc2::rc::Retained<objc2::runtime::NSObject>> =
        unsafe { objc2::msg_send![exception, reason] };
    string_from_nsobject(value)
}

/// An `NSString` field read out of an exception, as Rust text.
fn string_from_nsobject(
    value: Option<objc2::rc::Retained<objc2::runtime::NSObject>>,
) -> Option<String> {
    let value = value?;
    let utf8: *const c_char = unsafe { objc2::msg_send![&*value, UTF8String] };
    if utf8.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(utf8) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Whether an address is one of the guard's replacements.
///
/// For the tests, which check what the runtime holds against what the guard installed.
#[cfg(test)]
fn is_forwarder(address: usize) -> bool {
    (0..SLOTS).any(|slot| forwarder(slot) == Some(address))
}

/// Whether `observer` is one of AppKit's Touch Bar finder observations.
///
/// The class name is the only handle on a private class, so it is read at call time: the
/// finder and its observation class are created lazily, long after [`install`] ran.
fn is_finder_observer(observer: *mut Object) -> bool {
    class_name(observer).is_some_and(|name| is_finder_class_name(&name))
}

/// Whether a class name is one of AppKit's Touch Bar finder observations.
///
/// Total, and separated from the runtime lookup so that what it accepts can be read and
/// tested without a Touch Bar, or a debugger attached to AppKit.
fn is_finder_class_name(name: &str) -> bool {
    name.contains(FINDER_OBSERVER)
}

/// An object's class name, if it has a class at all.
fn class_name(object: *mut Object) -> Option<String> {
    if object.is_null() {
        return None;
    }
    let name: *const c_char = unsafe { object_getClassName(object) };
    if name.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// The key path an object stands for, for matching and the log.
fn key_path_name(key_path: *mut Object) -> String {
    if key_path.is_null() {
        return "<nil>".to_owned();
    }
    let utf8: *const c_char = unsafe { msg_send![key_path, UTF8String] };
    if utf8.is_null() {
        return "<not a string>".to_owned();
    }
    unsafe { CStr::from_ptr(utf8) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests;
