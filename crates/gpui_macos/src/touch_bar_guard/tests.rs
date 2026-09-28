//! The guard's own tests.
//!
//! See the module's documentation for what the guard is for. The probe runs in a
//! separate process because Objective-C exceptions cannot be unwound by Rust; see
//! probe_on_the_main_thread.

use std::ffi::c_void;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use objc::declare::ClassDecl;
use objc::runtime::Class;
use objc::{class, msg_send, sel, sel_impl};

use super::*;

#[test]
fn only_the_finders_own_observations_are_recognised() {
    assert!(is_finder_class_name("_NSTouchBarFinderObservation"));
    // A companion class, if AppKit ever grows one.
    assert!(is_finder_class_name("_NSTouchBarFinderObservationDummy"));
    // Everything else keeps Foundation's behaviour, exception included.
    assert!(!is_finder_class_name("NSView"));
    assert!(!is_finder_class_name("NSObject"));
    assert!(!is_finder_class_name(""));
}

/// Set in the child process that runs a probe, and which one it should run.
const PROBE_ENV: &str = "GPUI_MACOS_TOUCH_BAR_GUARD_PROBE";
/// Classes a retraction must reach the guard through.
///
/// `NSWindow` implements the method itself, while `NSView` and the field editor inherit
/// it: a guard that only patched `NSObject` fails this, which is how it failed when it
/// was first written.
const MUST_BE_COVERED: &[&str] = &[
    "NSObject",
    "NSResponder",
    "NSView",
    "NSControl",
    "NSWindow",
    "NSTextView",
    "NSTextField",
];
/// Installs the guard and retracts a finder observation twice.
const PROBE_GUARD: &str = "guard";
/// Installs the guard and retracts somebody else's observation twice.
const PROBE_FORWARD: &str = "forward";
/// Installs the guard and retracts somebody else's observation from a window twice.
const PROBE_WINDOW: &str = "window";
/// Installs the guard with nothing asking for it, and looks whether it stayed out.
const PROBE_OFF: &str = "off";

/// Runs the probe on the process's main thread.
///
/// An Objective-C exception raised on a plain thread cannot be unwound by Rust, and
/// libtest runs every test on a thread of its own, so a probe that raises one inside a
/// test kills the whole test binary. Instead the tests re-run this test binary with
/// [`PROBE_ENV`] set: this constructor runs on the main thread before libtest starts,
/// does the work, prints its verdict and exits. The tests then only read the child's
/// status — which is how the abduction below stays observable as a child killed by the
/// exception, rather than a test run that dies with it.
#[ctor::ctor(unsafe)]
fn probe_on_the_main_thread() {
    let Some(mode) = std::env::var_os(PROBE_ENV) else {
        return;
    };
    let (message, exit_code) = match mode.to_str() {
        Some(PROBE_GUARD) => match probe_guard() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        Some(PROBE_FORWARD) => match probe_forward() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        Some(PROBE_WINDOW) => match probe_window() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        Some(PROBE_OFF) => match probe_off() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        _ => ("probe: failed: unknown mode".to_owned(), 1),
    };
    println!("{message}");
    std::process::exit(exit_code);
}

/// `Ok("ok")` once every observed class retracts through the guard, `Err(…)` naming what
/// did not.
fn probe_guard() -> Result<&'static str, String> {
    install();

    let Some(replaced) = REPLACED.get().cloned() else {
        return Err("the guard replaced nothing".to_owned());
    };
    if replaced.is_empty() {
        return Err("the guard replaced nothing".to_owned());
    }

    // A class's replacement forwards to that class's implementation only if it has a slot
    // of its own: one shared replacement would have to guess the class the retraction
    // arrived through, and would guess the receiver on every window retraction.
    if replaced.len() != SLOTS_TAKEN.load(Ordering::Acquire) {
        return Err(format!(
            "{} classes were replaced but {} replacements were taken",
            replaced.len(),
            SLOTS_TAKEN.load(Ordering::Acquire)
        ));
    }
    for (class, ours) in &replaced {
        if *ours == 0 || !is_forwarder(*ours) {
            return Err(format!(
                "the class at {class:#x} was given an implementation that is not the guard's"
            ));
        }
    }
    for slot in 0..replaced.len() {
        if ORIGINALS[slot].load(Ordering::Acquire) == 0 {
            return Err(format!("slot {slot} has no original to forward to"));
        }
    }

    // Installing again must change nothing: the runtime is left with one replacement per
    // class, and no class ends up with the guard as its own original.
    install();
    if REPLACED.get() != Some(&replaced) {
        return Err("installing twice replaced something again".to_owned());
    }

    // The classes the finder actually observes. Their method may be their own or
    // inherited; either way it has to resolve to the guard.
    for name in MUST_BE_COVERED {
        let Some(class) = Class::get(name) else {
            return Err(format!("{name} is not registered"));
        };
        let Some(method) = class.instance_method(Sel::register(REMOVE_OBSERVER)) else {
            return Err(format!("{name} does not respond to {REMOVE_OBSERVER}"));
        };
        if !is_forwarder(method.implementation() as usize) {
            return Err(format!(
                "{name} still reaches Foundation's {REMOVE_OBSERVER}"
            ));
        }
    }

    // Views and the field editor inherit the method, so `NSObject` has to be in the
    // replaced set for their retractions to arrive here at all.
    let object_class = Class::get("NSObject").map(|class| class as *const Class);
    if !replaced
        .iter()
        .any(|(class, _)| Some(*class as *const Class) == object_class)
    {
        return Err("NSObject was not replaced, so neither are the views it serves".to_owned());
    }
    let observer = finder_observation()?;
    unsafe {
        let view: *mut Object = msg_send![class!(NSView), new];
        if view.is_null() {
            return Err("could not create a view to observe".to_owned());
        }
        retract_twice(view, observer);
    }

    Ok("ok")
}

/// `Err(…)` only if the guard also swallowed somebody else's retraction, which would be
/// a bug: a normal double retraction must still reach Foundation and throw. Otherwise
/// the uncaught exception kills the process before this returns.
fn probe_forward() -> Result<&'static str, String> {
    install();

    let observer: *mut Object = unsafe { msg_send![class!(NSObject), new] };
    let view: *mut Object = unsafe { msg_send![class!(NSView), new] };
    unsafe { retract_twice(view, observer) };

    Err("a retraction that is not the finder's was skipped too".to_owned())
}

/// Installs the guard and retracts somebody else's observation from a window twice.
///
/// `NSWindow` is the one AppKit responder that implements the method itself, and its
/// implementation is not the end of the chain: it hands the retraction on to
/// `NSObject`'s — which the guard has also replaced. A guard that finds the original by
/// the receiver's class, instead of by the class the retraction arrived through, calls
/// itself through the window's original until the stack runs out.
fn probe_window() -> Result<&'static str, String> {
    install();

    let observer: *mut Object = unsafe { msg_send![class!(NSObject), new] };
    let window: *mut Object = unsafe { msg_send![class!(NSWindow), new] };
    if window.is_null() {
        return Err("could not create a window to observe".to_owned());
    }
    unsafe { retract_twice(window, observer) };

    Err("a retraction that is not the finder's was skipped too".to_owned())
}

/// Installs the guard with nothing asking for it, and looks whether it stayed out.
///
/// Replacing methods of AppKit's own classes is not a free change — the module
/// documentation has the two crashes the field has to show for it — so it happens only
/// when [`GUARD_ENV`] says so.
fn probe_off() -> Result<&'static str, String> {
    install();
    if REPLACED.get().is_some() {
        return Err("the guard installed itself without being asked".to_owned());
    }
    Ok("off: nothing installed")
}

/// Registers `observer` for `nextResponder` on `object` and retracts it twice.
///
/// The second retraction is the one that raises `NSRangeException` and aborts the
/// process.
unsafe fn retract_twice(object: *mut Object, observer: *mut Object) {
    let key_path = unsafe { crate::ns_string("nextResponder") };
    let context: *mut c_void = std::ptr::null_mut();
    let _: () = unsafe {
        msg_send![object, addObserver: observer forKeyPath: key_path options: 0usize context: context]
    };
    let _: () = unsafe {
        msg_send![object, removeObserver: observer forKeyPath: key_path context: context]
    };
    let _: () = unsafe {
        msg_send![object, removeObserver: observer forKeyPath: key_path context: context]
    };
}

/// An instance of AppKit's Touch Bar finder observation class.
///
/// AppKit ships the class whether or not the machine has a Touch Bar, but it is private
/// and its `init` is not ours to call, so it is instantiated without one. If AppKit does
/// not have it (an older macOS), the probe declares its own class under the same name —
/// the guard only ever looks at the name.
fn finder_observation() -> Result<*mut Object, String> {
    if let Some(class) = Class::get(FINDER_OBSERVER) {
        let observer = unsafe { class_createInstance(class, 0) };
        if !observer.is_null() {
            return Ok(observer);
        }
        return Err("could not instantiate the finder's observation class".to_owned());
    }

    let Some(superclass) = Class::get("NSObject") else {
        return Err("NSObject is not registered".to_owned());
    };
    let Some(declaration) = ClassDecl::new(FINDER_OBSERVER, superclass) else {
        return Err(format!("{FINDER_OBSERVER} could not be declared"));
    };
    let class = declaration.register();
    let observer: *mut Object = unsafe { msg_send![class, new] };
    if observer.is_null() {
        return Err("could not instantiate the declared observation class".to_owned());
    }
    Ok(observer)
}

unsafe extern "C" {
    /// Not re-exported by the `objc` crate, which keeps its own declaration private.
    fn class_createInstance(class: *const Class, extra_bytes: usize) -> *mut Object;
}

/// The reason the guard exists: the finder's own retraction, twice, must not abort.
///
/// Only a Mac with a Touch Bar raises the exception in the field, but the retraction
/// reaches the same Foundation method everywhere, so the guard is checked everywhere.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn the_finders_retraction_leaves_nothing_to_throw() {
    let child = probe(PROBE_GUARD);
    assert!(
        child.status.success(),
        "the guard probe failed ({}):\n{}",
        child.status,
        child.output
    );
    assert!(
        child.output.contains("probe: ok"),
        "the guard probe did not finish:\n{}",
        child.output
    );
}

/// And the reason it is scoped: every other retraction still reaches Foundation.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn every_other_retraction_still_throws() {
    let child = probe(PROBE_FORWARD);
    assert!(
        !child.output.contains("was skipped too"),
        "a retraction that is not the finder's was swallowed:\n{}",
        child.output
    );
    assert!(
        !child.status.success(),
        "a retraction that is not the finder's did not throw:\n{}",
        child.output
    );
    assert!(
        child.status.code().is_none(),
        // Killed by the uncaught exception, not exited by the probe.
        "the child exited instead of being killed by the exception: {}",
        child.status
    );
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn installing_twice_leaves_the_runtime_alone() {
    // `install` is called once per process, but must be harmless if it is called again:
    // the probe checks, in a process of its own, that the second call changed nothing.
    let child = probe(PROBE_GUARD);
    assert!(
        child.output.contains("probe: ok"),
        "the guard probe did not finish:\n{}",
        child.output
    );
}

/// A retraction from a window has to reach Foundation as well.
///
/// `NSWindow` implements the method itself and hands the retraction on to `NSObject`'s,
/// which is the one case where one shared replacement would call itself through the
/// window's original until the stack runs out. The child dies by the exception's abort
/// (`SIGABRT`), not by the stack overflow's fault (`SIGSEGV`), which is what the first
/// version of the guard did on a tester's Mac — see the module documentation.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn a_window_retraction_does_not_call_the_guard_in_a_circle() {
    let child = probe(PROBE_WINDOW);
    assert!(
        !child.output.contains("was skipped too"),
        "a retraction that is not the finder's was swallowed:\n{}",
        child.output
    );
    assert_eq!(
        child.status.signal(),
        Some(6),
        // Six is `SIGABRT`: the uncaught exception. A stack overflow arrives as eleven,
        // `SIGSEGV`, which is the circular call above.
        "the window's retraction did not reach Foundation: {}\n{}",
        child.status,
        child.output
    );
}

/// And nothing installs itself unless the environment asks for it.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn the_guard_is_off_unless_the_environment_asks_for_it() {
    // What counts as asking for it, read without setting anything.
    assert!(guard_requested_by("1"));
    assert!(guard_requested_by("on"));
    assert!(guard_requested_by(" TRUE "));
    assert!(!guard_requested_by(""));
    assert!(!guard_requested_by("0"));
    assert!(!guard_requested_by("false"));
    assert!(!guard_requested_by("off"));

    // And nothing is replaced in a process where it was not asked for.
    let child = probe_without_guard(PROBE_OFF);
    assert!(
        child.output.contains("probe: off: nothing installed"),
        "the guard did not stay out of the way:\n{}",
        child.output
    );
}

/// The child's status and its combined output.
struct ProbeResult {
    status: std::process::ExitStatus,
    output: String,
}

/// Runs the probe in a child of this test binary, which gives it a main thread, and with
/// the guard asked for.
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn probe(mode: &str) -> ProbeResult {
    probe_with(mode, Some("1"))
}

/// Runs the probe with [`GUARD_ENV`] left unset.
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn probe_without_guard(mode: &str) -> ProbeResult {
    probe_with(mode, None)
}

/// Runs the probe, with the guard asked for or not, and collects its status and output.
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn probe_with(mode: &str, guard: Option<&str>) -> ProbeResult {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command.env(PROBE_ENV, mode).env_remove(GUARD_ENV);
    if let Some(value) = guard {
        command.env(GUARD_ENV, value);
    }
    let child = command.output().expect("run the touch bar guard probe");
    let mut output = String::from_utf8_lossy(&child.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&child.stderr));
    ProbeResult {
        status: child.status,
        output,
    }
}
