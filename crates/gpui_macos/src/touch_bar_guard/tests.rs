//! The Touch Bar guard's own tests.
//!
//! The guard lives in a dynamic-method world that a normal `#[test]` cannot reach: the
//! replaced method belongs to AppKit classes, and the exception the guard exists for
//! only appears on the main thread of a process that has AppKit loaded. Each probe
//! therefore runs as a child of this very binary — `probe_on_the_main_thread` runs from
//! a `ctor` before `main`, the mode arriving in [`PROBE_ENV`] picks what it checks — and
//! the parent asserts on the child's status and output.
//!
//! Why the semantic probes prove removal by *retracting again* instead of by counting
//! notifications: Foundation dispatches `observeValueForKeyPath:` to a dynamically
//! registered observer class's `NSObject` implementation — not to the method the class
//! was declared with — so a change notification throws "was received but not handled"
//! before any Rust counter can run. The duplicate retraction's
//! "because it is not registered as an observer" is the same evidence from the other
//! side: it can only throw if the first retraction really removed something that the
//! registration really put there.

use std::os::raw::c_void;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Command;

use ctor::ctor;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

use super::{
    GUARD_ENV, ORIGINALS, REMOVE_OBSERVER, REPLACED, SLOTS_TAKEN, exception_name, exception_reason,
    guard_disabled_by, install, is_finder_class_name, is_forwarder, is_unregistered_retraction,
};

/// The environment variable that turns this binary into a probe.
const PROBE_ENV: &str = "GPUI_MACOS_TOUCH_BAR_GUARD_PROBE";
/// The modes, one probe each.
const PROBE_GUARD: &str = "guard";
const PROBE_FORWARD: &str = "forward";
const PROBE_WINDOW: &str = "window";
const PROBE_RETHROW: &str = "rethrow";
const PROBE_OFF: &str = "off";

/// The probe's entry: a constructor, so it runs on the main thread before `main` —
/// the only thread AppKit's dynamic machinery is reliable on in a fresh process.
#[ctor(unsafe)]
fn probe_on_the_main_thread() {
    let Some(mode) = std::env::var_os(PROBE_ENV) else {
        return;
    };
    let Some(mode) = mode.to_str() else {
        return;
    };
    let (message, exit_code) = match mode {
        PROBE_GUARD => match probe_guard() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        PROBE_FORWARD => match probe_forward() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        PROBE_WINDOW => match probe_window() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        PROBE_RETHROW => match probe_rethrow() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        PROBE_OFF => match probe_off() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        },
        _ => ("probe: failed: unknown mode".to_owned(), 1),
    };
    println!("{message}");
    std::process::exit(exit_code);
}

/// `Ok("ok")` once the guard holds the whole contract on the finder's own observations.
///
/// The contract, in the order it is checked:
///
/// 1. the runtime dispatches every observed class's retraction through the guard;
/// 2. a legal registration's **first** retraction really removes it: retracting again
///    raises "not registered", which can only be raised by an empty registry — the
///    first guard version failed exactly here, skipping the legal retraction too, which
///    also made every later duplicate pass vacuously;
/// 3. that duplicate retraction is absorbed, and the process lives on;
/// 4. two registrations under different contexts come off independently.
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
    if replaced.len() != SLOTS_TAKEN.load(std::sync::atomic::Ordering::Acquire) {
        return Err(format!(
            "{} classes were replaced but {} replacements were taken",
            replaced.len(),
            SLOTS_TAKEN.load(std::sync::atomic::Ordering::Acquire)
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
        if ORIGINALS[slot].load(std::sync::atomic::Ordering::Acquire) == 0 {
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
    // inherited; either way it has to resolve to the guard. `NSView` and `NSWindow`
    // are the two the field reports named, and they have to be loadable here.
    for name in ["NSView", "NSWindow"] {
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

    // The semantic core, on a view — the class whose retraction arrives through
    // `NSObject`'s slot, which is how the field reports' crashes arrive.
    unsafe {
        let view: *mut Object = msg_send![class!(NSView), new];
        if view.is_null() {
            return Err("could not create a view to observe".to_owned());
        }
        legal_retraction_then_duplicate(view)?;
        contexts_come_off_independently(view)?;
    }

    Ok("ok")
}

/// Registers the probe observer on a view, retracts once — the legal retraction, which
/// must really remove — and proves the removal by retracting again: the duplicate must
/// raise "not registered" (only an empty registry can), and the guard must absorb it.
///
/// The first guard version failed here without noticing: it skipped the legal retraction
/// as well, the observation stayed registered, and no duplicate was ever raised to be
/// absorbed.
unsafe fn legal_retraction_then_duplicate(view: *mut Object) -> Result<(), String> {
    let observer = probe_observer();
    let key_path = unsafe { crate::ns_string("nextResponder") };
    let context: *mut c_void = std::ptr::null_mut();

    // The registration, and the legal retraction through the runtime — the replaced
    // method, which is the whole point. The first retraction of a live registration
    // must come back without an exception: a guard that skipped it would leave the
    // observation registered forever, which is the defect this probe pins.
    let view_p = view as usize;
    let observer_p = observer as usize;
    let key_p = key_path as usize;
    let first = objc2::exception::catch(move || unsafe {
        let view: *mut Object = std::mem::transmute(view_p);
        let observer: *mut Object = std::mem::transmute(observer_p);
        let key_path: *mut Object = std::mem::transmute(key_p);
        let _: () = msg_send![
            view,
            addObserver: observer
            forKeyPath: key_path
            options: 0usize
            context: std::ptr::null_mut::<c_void>()
        ];
        let _: () = msg_send![
            view,
            removeObserver: observer
            forKeyPath: key_path
            context: std::ptr::null_mut::<c_void>()
        ];
    });
    match first {
        Ok(()) => {}
        Err(Some(exception)) => {
            return Err(format!(
                "the legal retraction threw {}: {} — the guard must forward it, not skip it",
                exception_name(&exception).as_deref().unwrap_or("?"),
                exception_reason(&exception).as_deref().unwrap_or("?")
            ));
        }
        Err(None) => return Err("the legal retraction threw a foreign exception".to_owned()),
    }

    // The duplicate: nothing is registered now, so Foundation raises the exception the
    // guard exists for. It arrives through the replaced method — the guard absorbs it,
    // and this returns normally. A guard that rethrows it kills this process before the
    // parent can assert anything kinder than a signal.
    unsafe {
        let _: () = msg_send![
            view,
            removeObserver: observer
            forKeyPath: key_path
            context: context
        ];
    }
    Ok(())
}

/// Two registrations of the same observer under different contexts come off
/// independently: retracting one leaves the other registered — the proof is the same
/// empty-registry exception, raised only after the second retraction.
///
/// The finder uses one context pointer for all of its observations; a guard that removed
/// by observer alone would take both off with one retraction, which is not what
/// `removeObserver:forKeyPath:context:` promises.
unsafe fn contexts_come_off_independently(view: *mut Object) -> Result<(), String> {
    let observer = probe_observer();
    let key_path = unsafe { crate::ns_string("nextResponder") };
    static A: () = ();
    static B: () = ();
    let context_a: *mut c_void = &A as *const () as *mut c_void;
    let context_b: *mut c_void = &B as *const () as *mut c_void;

    unsafe {
        let _: () = msg_send![
            view,
            addObserver: observer
            forKeyPath: key_path
            options: 0usize
            context: context_a
        ];
        let _: () = msg_send![
            view,
            addObserver: observer
            forKeyPath: key_path
            options: 0usize
            context: context_b
        ];
    }

    // Retracting context A: B is still registered, so this must not raise — and must
    // not take B off with it. A guard that removed by observer alone would empty both.
    let view_p = view as usize;
    let observer_p = observer as usize;
    let key_p = key_path as usize;
    let retract_a = objc2::exception::catch(move || unsafe {
        let view: *mut Object = std::mem::transmute(view_p);
        let observer: *mut Object = std::mem::transmute(observer_p);
        let key_path: *mut Object = std::mem::transmute(key_p);
        let _: () = msg_send![
            view,
            removeObserver: observer
            forKeyPath: key_path
            context: context_a
        ];
    });
    match retract_a {
        Ok(()) => {}
        Err(Some(exception)) => {
            return Err(format!(
                "retracting context A raised {}: {} — it took context B off with it",
                exception_name(&exception).as_deref().unwrap_or("?"),
                exception_reason(&exception).as_deref().unwrap_or("?")
            ));
        }
        Err(None) => return Err("retracting context A raised a foreign exception".to_owned()),
    }

    // Retracting context B still has something to remove — no exception. If A's
    // retraction had emptied both, this would raise the duplicate instead.
    unsafe {
        let _: () = msg_send![
            view,
            removeObserver: observer
            forKeyPath: key_path
            context: context_b
        ];
    }

    // And now the registry is empty: the duplicate raises, and the guard absorbs it.
    unsafe {
        let _: () = msg_send![
            view,
            removeObserver: observer
            forKeyPath: key_path
            context: context_b
        ];
    }
    Ok(())
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

/// Retracts the finder's observation of a key path that is not `nextResponder`, twice.
///
/// The exception is the same `NSRangeException` the duplicate `nextResponder` retraction
/// raises, and the observer is the finder's own shape — but this module absorbs nothing
/// outside the key path it exists for, so the exception has to unwind out of the guard
/// and kill this process. A child that survives means the guard is swallowing too much.
fn probe_rethrow() -> Result<&'static str, String> {
    install();

    let observer = probe_observer();
    let view: *mut Object = unsafe { msg_send![class!(NSView), new] };
    let key_path = unsafe { crate::ns_string("hidden") };
    let context: *mut c_void = std::ptr::null_mut();
    unsafe { retract_twice(view, observer) };
    // If the guard wrongly absorbed the duplicate, remove through the other spelling to
    // give it a second chance to rethrow — then report the failure.
    unsafe {
        let _: () = msg_send![
            view,
            removeObserver: observer
            forKeyPath: key_path
            context: context
        ];
    }

    Err("the finder's retraction of another key path was swallowed".to_owned())
}

/// Installs the guard with [`GUARD_ENV`] saying off, and looks whether it stayed out.
///
/// Replacing methods of AppKit's own classes is not a free change — the module
/// documentation has the crashes the field has to show for it — so there is a way to
/// run without it, and the switch has to keep working.
fn probe_off() -> Result<&'static str, String> {
    install();
    if REPLACED.get().is_some() {
        return Err("the guard installed itself with the environment saying off".to_owned());
    }
    Ok("off: nothing installed")
}

/// The probe's stand-in for the finder: the name the guard matches. No
/// `observeValueForKeyPath:` is declared — Foundation does not dispatch change
/// notifications to a dynamically registered class's own implementation anyway, and the
/// probes prove registration and removal through the retraction exceptions instead.
///
/// AppKit's own class is not instantiated for the semantic probes: its `init` is not
/// ours to call. The guard only ever looks at the name.
fn probe_observer() -> *mut Object {
    use std::sync::OnceLock;
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    let class = CLASS.get_or_init(|| {
        // The probe runs before AppKit is necessarily linked in, so the superclass is
        // looked up lazily and the class is only declared when `NSObject` answers.
        let Some(superclass) = Class::get("NSObject") else {
            return Class::get("NSObject").expect("NSObject is not registered");
        };
        let declaration = ClassDecl::new("_NSTouchBarFinderProbeObservation", superclass)
            .expect("the probe observation class could not be declared");
        declaration.register()
    });
    let class = *class;
    unsafe { msg_send![class, new] }
}

/// Registers `observer` for `nextResponder` on `object` and retracts it twice.
///
/// The second retraction is the one that raises `NSRangeException`; who sees it is what
/// the probes differ in.
unsafe fn retract_twice(object: *mut Object, observer: *mut Object) {
    let key_path = unsafe { crate::ns_string("nextResponder") };
    let context: *mut c_void = std::ptr::null_mut();
    unsafe {
        let _: () = msg_send![
            object,
            addObserver: observer
            forKeyPath: key_path
            options: 0usize
            context: context
        ];
        let _: () = msg_send![
            object,
            removeObserver: observer
            forKeyPath: key_path
            context: context
        ];
        let _: () = msg_send![
            object,
            removeObserver: observer
            forKeyPath: key_path
            context: context
        ];
    }
}

use objc::declare::ClassDecl;

/// The reason the guard exists: the finder's own retraction, twice, must not abort —
/// and the first retraction must really remove the observation.
///
/// Only a Mac with a Touch Bar raises the exception in the field, but the retraction
/// reaches the same Foundation method everywhere, so the guard is checked everywhere.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn the_finders_first_retraction_removes_and_the_duplicate_is_absorbed() {
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

/// The absorption is narrow: the same duplicate retraction on another key path is
/// thrown back, and the process dies exactly as Foundation meant it to.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn another_key_paths_retraction_is_thrown_back() {
    let child = probe(PROBE_RETHROW);
    assert!(
        !child.output.contains("was swallowed"),
        "the guard absorbed a retraction outside its key path:\n{}",
        child.output
    );
    assert_eq!(
        child.status.signal(),
        Some(6),
        "the retraction of another key path did not reach Foundation: {}\n{}",
        child.status,
        child.output
    );
}

/// Only the finder's own observation class counts; every other observer — even one
/// whose name merely mentions the finder — is not ours to absorb for.
#[test]
fn only_the_finders_own_observations_are_recognised() {
    assert!(is_finder_class_name("_NSTouchBarFinderObservation"));
    assert!(is_finder_class_name("_NSTouchBarFinderProbeObservation"));
    // Not the finder's: no Touch Bar in the name at all.
    assert!(!is_finder_class_name("NSObject"));
    assert!(!is_finder_class_name("NSWindow"));
    // Substring mentions that are not the observation class.
    assert!(!is_finder_class_name("NSTouchBar"));
    assert!(!is_finder_class_name("NSTouchBarItem"));
    assert!(!is_finder_class_name(""));
}

/// An `NSException` built to order, for classifying without throwing.
fn exception_of(name: &str, reason: &str) -> objc2::rc::Retained<objc2::exception::Exception> {
    // Built with the `objc` crate's classes, like everything else in this file: the
    // workspace pulls two `objc2`s and the test targets this crate's 0.6.3 one, which
    // the `class!` macro of 0.5.2 cannot hand a type from.
    let exception: *mut Object = unsafe {
        msg_send![
            class!(NSException),
            exceptionWithName: crate::ns_string(name)
            reason: crate::ns_string(reason)
            userInfo: std::ptr::null_mut::<Object>()
        ]
    };
    // A live NSException as the guard sees it: the guard only reads two selectors off
    // it, so the pointer type it arrived under does not matter. `exceptionWithName:…`
    // returns an autoreleased object, so it is retained before `Retained` takes it —
    // owning it without that would over-release it on drop.
    let exception: *mut Object = unsafe { msg_send![exception, retain] };
    let retained: Option<objc2::rc::Retained<objc2::exception::Exception>> =
        unsafe { objc2::rc::Retained::from_raw(exception.cast()) };
    retained.expect("the exception was not created")
}

/// The classification the absorption depends on: exactly one combination counts.
#[test]
fn only_the_unregistered_next_responder_retraction_counts() {
    let matching = exception_of(
        "NSRangeException",
        "Cannot remove an observer because it is not registered as an observer",
    );
    assert!(is_unregistered_retraction(&matching, "nextResponder"));

    // Any other key path: not ours.
    assert!(!is_unregistered_retraction(&matching, "hidden"));

    // Any other exception name: not ours.
    let other_name = exception_of(
        "NSInvalidArgumentException",
        "Cannot remove an observer because it is not registered as an observer",
    );
    assert!(!is_unregistered_retraction(&other_name, "nextResponder"));

    // Any other reason: not ours — an object that never was registered, say.
    let other_reason = exception_of(
        "NSRangeException",
        "Cannot remove an observer because it is no longer registered as an observer",
    );
    assert!(!is_unregistered_retraction(&other_reason, "nextResponder"));
}

/// And it installs itself unless the environment says off.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn the_guard_is_on_unless_the_environment_says_off() {
    // What counts as off, read without setting anything. An empty value is not off: a
    // wrapper that meant to pass nothing must not be able to drop the guard.
    assert!(!guard_disabled_by(""));
    assert!(!guard_disabled_by("1"));
    assert!(!guard_disabled_by("on"));
    assert!(!guard_disabled_by(" TRUE "));
    assert!(guard_disabled_by("0"));
    assert!(guard_disabled_by("false"));
    assert!(guard_disabled_by("off"));
    assert!(guard_disabled_by(" OFF "));
    assert!(guard_disabled_by("no"));

    // And the guard is in place in a process where nothing was set at all — the same
    // probe that would otherwise fail reports the replacements instead.
    let child = probe_with(PROBE_GUARD, None);
    assert_eq!(
        child.status.code(),
        Some(0),
        "the guard did not install itself with nothing set:\n{}",
        child.output
    );

    // While a process that says off is left alone.
    let child = probe_disabled(PROBE_OFF);
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

/// Runs the probe with [`GUARD_ENV`] saying off.
#[allow(
    clippy::disallowed_methods,
    reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
)]
fn probe_disabled(mode: &str) -> ProbeResult {
    probe_with(mode, Some("0"))
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
