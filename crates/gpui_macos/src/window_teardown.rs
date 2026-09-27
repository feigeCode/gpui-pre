//! What happens to a native window once GPUI is done with it.
//!
//! The window is created with `alloc` + `initWithContentRect_…` and
//! `setReleasedWhenClosed: NO`, so `MacWindow::drop` owns the single release of
//! that reference. It must not perform it, though.
//!
//! On machines with a Touch Bar, AppKit's `_NSTouchBarFinder` installs KVO
//! observations on the window's views — AppKit's own frame view included — and
//! retracts them later, from `-[_NSTouchBarFinderObservation invalidate]`. When
//! the observed view is already gone, that call throws
//!
//! ```text
//! Cannot remove an observer <_NSTouchBarFinderObservation 0x…> for the key path
//! "nextResponder" from <NSView 0x…> because it is not registered as an observer.
//! ```
//!
//! nothing catches it, and `NSApplication _crashOnException:` aborts the process
//! (SIGABRT directly, or SIGILL through the invalid instruction trap).
//!
//! The retraction is not tied to the close, or to the next display cycle: it
//! happens whenever AppKit next walks the responder chain, and the throw lands in
//! AppKit's own display-cycle flush rather than on our call stack. So neither
//! waiting before the release — a 100 ms grace period was shipped and windows
//! still aborted — nor catching the exception at the call site can make releasing
//! the window safe. Only keeping the views alive does.
//!
//! Hence [`retire`]: the native window is closed and taken off screen as usual, but
//! never released. Everything else is released instead:
//!
//! * `MacWindowState::retire` drops the renderer — and with it the metal layer, its
//!   drawable pool and the renderer's textures — plus the accessibility adapter and
//!   every callback, which capture GPUI entities and would keep the closed window's
//!   content alive.
//! * [`release_layer`] takes the `GPUIView` out of layer-backing, which is what
//!   releases the metal layer it was still hosting on the renderer's behalf.
//!
//! What stays is an off-screen window with an empty view hierarchy and a state that
//! still answers every native entry point, one entry per window the app ever closed.
//! That retention is deliberate and only bounded by window churn, not a fix for it:
//! releasing the window later is what aborts, and reusing one for a new GPUI window
//! would mean re-initializing AppKit state this module would have to keep in sync.
//! Windows are created and closed rarely enough that what is retained (an `NSWindow`
//! and one layer-less view, a few KB, no GPU resources) is small next to the renderer
//! that used to be held per closed window; [`RETIRED_WINDOWS_WARN_THRESHOLD`] is where
//! to look if that ever stops being true.
//!
//! Upstream report with the full abort backtrace:
//! <https://github.com/zed-industries/zed/issues/64819>.

use cocoa::base::{id, nil};
use objc::runtime::NO;
use objc::{msg_send, sel, sel_impl};
use parking_lot::Mutex;

/// How many retired native windows in one app session are still unremarkable.
///
/// Reaching it is reported, not acted on: releasing or recycling a retired window is
/// exactly what this module exists to avoid. It is a number to look at when memory is
/// suspected to grow with window churn — see the module docs.
const RETIRED_WINDOWS_WARN_THRESHOLD: usize = 32;

/// Native windows that were closed but deliberately never released.
///
/// Held as `usize` because `id` is not `Send` and nothing ever reads them back: the
/// list exists only so that the references it owns are never dropped.
static RETIRED_WINDOWS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Keeps `window` — and with it every view AppKit may still be observing — alive,
/// instead of releasing the reference `MacWindow::drop` owns.
///
/// [`release_layer`] runs first, so the retired window no longer holds the renderer's
/// metal layer.
///
/// # Safety
///
/// `window` must be a window GPUI created with `alloc`/`init…` plus
/// `setReleasedWhenClosed: NO`, whose caller gives up its reference; `view` must be
/// that window's `GPUIView`, and both must still be alive. Must be called on the main
/// thread, with the window already closed.
pub(crate) unsafe fn retire(window: id, view: id) {
    unsafe {
        release_layer(view);
    }

    let mut retired = RETIRED_WINDOWS.lock();
    retired.push(window as usize);
    if retired.len() == RETIRED_WINDOWS_WARN_THRESHOLD {
        log::warn!(
            "retired {RETIRED_WINDOWS_WARN_THRESHOLD} native windows; they are kept alive \
             on purpose (see gpui_macos::window_teardown), but if memory is growing with \
             window churn this is where to look"
        );
    } else {
        log::debug!("retired a native window, {} in total", retired.len());
    }
}

/// How many native windows have been retired in this process. Used by the tests below.
#[cfg(test)]
pub(crate) fn retired_window_count() -> usize {
    RETIRED_WINDOWS.lock().len()
}

/// Takes `view` out of layer-backing, releasing the metal layer that the window's
/// renderer had handed to AppKit from `makeBackingLayer`.
///
/// The renderer is already gone by the time a window is retired, so this view — and
/// the layer it was still hosting — is the last thing holding that layer, its drawable
/// pool and its textures.
///
/// `setWantsLayer: NO` is not enough on its own: it only asks AppKit to drop the layer,
/// and AppKit does so on the view's next display cycle — which a retired window, being
/// off screen, never gets. Detaching the layer is what releases it. The probe in the
/// tests below asserts the outcome rather than either of these two calls.
///
/// The window's `NSVisualEffectView` (if the window has a transparent titlebar) is
/// left alone on purpose: AppKit requires that class to stay layer-backed.
///
/// # Safety
///
/// `view` must be a live `GPUIView`, on the main thread.
unsafe fn release_layer(view: id) {
    unsafe {
        let _: () = msg_send![view, setWantsLayer: NO];

        let layer: id = msg_send![view, layer];
        if !layer.is_null() {
            // `setWantsLayer: NO` only asks AppKit to drop the layer it made for this
            // view, and AppKit does that on the view's next display cycle. A retired
            // window is off screen and never gets one, so the layer — and the drawable
            // pool and textures hanging off it — would stay. Detaching it here is what
            // actually releases it, and the probe in the tests below asserts it.
            let _: () = msg_send![view, setLayer: nil];
        }

        let layer: id = msg_send![view, layer];
        if !layer.is_null() {
            // Not fatal — the window is off screen and no longer draws — but it means
            // the layer outlived the renderer, so the release chain needs another look.
            log::warn!("a retired window's view still holds a layer");
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the invariant this module exists for: a retired window's native
    //! objects stay alive, and the renderer's metal layer does not stay with them.
    //!
    //! They run on any macOS, on any architecture. The abort itself needs a Touch Bar
    //! (`_NSTouchBarFinder` only observes anything when one is attached), which no CI
    //! runner has, but what makes retiring safe is not Touch Bar specific: what is
    //! asserted here is the state the finder needs to find intact.

    use super::*;
    use cocoa::appkit::{NSBackingStoreBuffered, NSWindowStyleMask};
    use cocoa::foundation::{NSPoint, NSRect, NSSize};
    use objc::declare::ClassDecl;
    use objc::runtime::{BOOL, Class, Object, Sel, YES};
    use objc::{class, msg_send, sel, sel_impl};
    use std::sync::OnceLock;

    /// A view shaped like `GPUIView`: layer-backed, with the backing layer coming from
    /// `makeBackingLayer` — the layer a renderer hands to AppKit.
    fn probe_view_class() -> &'static Class {
        static CLASS: OnceLock<&'static Class> = OnceLock::new();
        CLASS.get_or_init(|| {
            let mut decl = ClassDecl::new("GPUIWindowTeardownProbeView", class!(NSView)).unwrap();
            unsafe {
                decl.add_method(
                    sel!(makeBackingLayer),
                    make_probe_backing_layer as extern "C" fn(&Object, Sel) -> id,
                );
            }
            decl.register()
        })
    }

    extern "C" fn make_probe_backing_layer(_: &Object, _: Sel) -> id {
        unsafe {
            let layer: id = msg_send![class!(CALayer), alloc];
            msg_send![layer, init]
        }
    }

    fn probe_frame() -> NSRect {
        NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 200.0,
                height: 200.0,
            },
        }
    }

    /// A window with a layer-backed probe view in it, created the way GPUI creates one:
    /// `setReleasedWhenClosed: NO`, so the one reference belongs to the caller.
    ///
    /// `None` means this process has no window server connection (a headless runner,
    /// say), which says nothing about retirement.
    fn layer_backed_window() -> Option<(id, id)> {
        unsafe {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            let frame = probe_frame();
            let style_mask = NSWindowStyleMask::NSTitledWindowMask;
            let window: id = msg_send![class!(NSWindow), alloc];
            let window: id = msg_send![
                window,
                initWithContentRect: frame
                styleMask: style_mask
                backing: NSBackingStoreBuffered
                defer: NO
            ];
            if app.is_null() || window.is_null() {
                return None;
            }
            let _: () = msg_send![window, setReleasedWhenClosed: NO];

            let view: id = msg_send![probe_view_class(), alloc];
            let view: id = msg_send![view, initWithFrame: frame];
            let _: () = msg_send![view, setWantsLayer: YES];
            let content_view: id = msg_send![window, contentView];
            let _: () = msg_send![content_view, addSubview: view];
            let _: () = msg_send![window, displayIfNeeded];
            Some((window, view))
        }
    }

    /// Set in the child process that runs the probe.
    const PROBE_ENV: &str = "GPUI_MACOS_TEARDOWN_PROBE";

    /// Runs the probe on the process's main thread.
    ///
    /// Anything involving a window has to happen on the main thread: libtest runs every
    /// test on a thread of its own, and creating and displaying a window there throws an
    /// Objective-C exception that Rust cannot unwind, which aborts the process. So the
    /// probe does not run inside a test. The test below re-runs this test binary with
    /// [`PROBE_ENV`] set, this constructor — which runs on the main thread, before
    /// libtest starts — does the work and exits, and the test only reads the child's
    /// exit status. That is also what makes an abort observable: it arrives as a failed
    /// child with a signal, instead of a test that never finishes.
    #[ctor::ctor(unsafe)]
    fn probe_on_the_main_thread() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }

        let (message, exit_code) = match probe() {
            Ok(message) => (format!("probe: {message}"), 0),
            Err(message) => (format!("probe: failed: {message}"), 1),
        };
        println!("{message}");
        std::process::exit(exit_code);
    }

    /// `Ok("ok")` once every guarantee holds, `Ok("skipped: …")` where this process
    /// cannot have a window at all, `Err(…)` naming the first guarantee that fails.
    fn probe() -> Result<&'static str, String> {
        unsafe {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            let Some((window, view)) = layer_backed_window() else {
                return Ok("skipped: this process is not connected to a window server");
            };
            if app.is_null() {
                return Ok("skipped: this process is not connected to a window server");
            }

            let layer: id = msg_send![view, layer];
            if layer.is_null() {
                return Err("the probe view is not layer-backed, so nothing was proven".to_owned());
            }

            // Closed the way GPUI closes a window, and only then retired.
            let _: () = msg_send![window, close];

            let before = retired_window_count();
            retire(window, view);

            if retired_window_count() != before + 1 {
                return Err("the window was not recorded as retired".to_owned());
            }
            let layer_after: id = msg_send![view, layer];
            if !layer_after.is_null() {
                return Err("a retired window's view still holds the renderer's layer".to_owned());
            }

            // Both objects must still answer: they are what AppKit's Touch Bar finder
            // may still be observing when it retracts its observations.
            let is_view: BOOL = msg_send![view, isKindOfClass: class!(NSView)];
            if is_view != YES {
                return Err("the retired window's view is gone".to_owned());
            }
            let released_when_closed: BOOL = msg_send![window, isReleasedWhenClosed];
            if released_when_closed != NO {
                return Err("the retired window is gone, or was released after all".to_owned());
            }
            let content_view: id = msg_send![window, contentView];
            let subviews: id = msg_send![content_view, subviews];
            let subview_count: usize = msg_send![subviews, count];
            if subview_count != 1 {
                return Err(format!(
                    "the retired window has {subview_count} subviews, not the one AppKit observes"
                ));
            }

            // A second retired window must not displace the first: the list owns
            // references, it is not a pool that hands them back.
            let Some((second_window, second_view)) = layer_backed_window() else {
                return Err("could not create a second probe window".to_owned());
            };
            let _: () = msg_send![second_window, close];
            retire(second_window, second_view);
            if retired_window_count() != before + 2 {
                return Err("retiring a second window did not keep both".to_owned());
            }
            let is_view: BOOL = msg_send![view, isKindOfClass: class!(NSView)];
            if is_view != YES {
                return Err("retiring a second window released the first".to_owned());
            }
        }

        Ok("ok")
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the probe has to be a separate process to get a main thread, and blocking a test thread costs nothing"
    )]
    fn retiring_keeps_the_native_objects_alive_and_releases_the_view_layer() {
        let child = std::process::Command::new(std::env::current_exe().expect("test binary path"))
            .env(PROBE_ENV, "1")
            .output()
            .expect("run the teardown probe");
        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            child.status.success(),
            "the teardown probe failed ({}):\n{stdout}\n{stderr}",
            child.status
        );
        // A skipped probe would leave the guarantees unchecked, so it is a failure too.
        assert!(
            stdout.contains("probe: ok"),
            "the teardown probe did not check anything:\n{stdout}\n{stderr}"
        );
    }
}
