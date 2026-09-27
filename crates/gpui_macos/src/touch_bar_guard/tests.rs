//! The guard's own tests.
//!
//! See the module's documentation for what the guard is for. The probe runs in a
//! separate process because Objective-C exceptions cannot be unwound by Rust; see
//! probe_on_the_main_thread.

use std::ffi::c_void;
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
        _ => ("probe: failed: unknown mode".to_owned(), 1),
    };
    println!("{message}");
    std::process::exit(exit_code);
}

/// `Ok("ok")` once every observed class retracts through the guard, `Err(…)` naming what
/// did not.
fn probe_guard() -> Result<&'static str, String> {
    install();

    let Some(replaced) = REPLACED.get() else {
        return Err("the guard replaced nothing".to_owned());
    };
    if replaced.is_empty() {
        return Err("the guard replaced nothing".to_owned());
    }
    let ours = our_implementation();
    for (class, original) in replaced {
        if *original == 0 || *original == ours {
            return Err(format!(
                "the class at {class:#x} was given the guard as its own implementation"
            ));
        }
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
        if method.implementation() as usize != ours {
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
fn installing_is_idempotent_and_keeps_one_original_per_class() {
    let child = probe(PROBE_GUARD);
    assert!(
        child.output.contains("probe: ok"),
        "the guard probe did not finish:\n{}",
        child.output
    );

    // `install` is called once per process, but must be harmless if it is called again:
    // the guard must never record itself as what it replaced.
    install();
    let replaced = REPLACED
        .get()
        .expect("the probe process installed the guard")
        .clone();
    install();
    assert_eq!(REPLACED.get(), Some(&replaced));
}

/// The child's status and its combined output.
struct ProbeResult {
    status: std::process::ExitStatus,
    output: String,
}

/// Runs the probe in a child of this test binary, which gives it a main thread.
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn probe(mode: &str) -> ProbeResult {
    let child = Command::new(std::env::current_exe().expect("test binary path"))
        .env(PROBE_ENV, mode)
        .output()
        .expect("run the touch bar guard probe");
    let mut output = String::from_utf8_lossy(&child.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&child.stderr));
    ProbeResult {
        status: child.status,
        output,
    }
}
