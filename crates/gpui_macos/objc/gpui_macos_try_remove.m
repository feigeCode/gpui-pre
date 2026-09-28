// Don't include any headers, cross compilation is difficult to set up
// properly in such situations. Everything used here is in libobjc.

#ifndef NULL
#define NULL ((void *)0)
#endif

id objc_retain(id value);

/// The IMP shape of `removeObserver:forKeyPath:context:`.
typedef void (*Remover)(id self, SEL _cmd, id observer, id key_path, void *context);

/// Calls `imp` inside an Objective-C `@try`/`@catch`, with **no Rust frame between
/// the raise and the `@catch`**.
///
/// That is the point of this shim: `objc2::exception::catch` runs a Rust closure
/// inside its `@try`, and a Rust frame compiled with `panic = abort` turns a
/// passing Objective-C unwind into `panic in a function that cannot unwind` —
/// which is the abort this guard exists to prevent. Here the `@try` body calls
/// the original implementation directly, so the unwind never crosses a Rust
/// frame, whatever the panic strategy of the surrounding build is.
///
/// Returns the retained exception, or NULL when nothing was raised.
id gpui_macos_try_remove(Remover imp,
                         id self,
                         SEL _cmd,
                         id observer,
                         id key_path,
                         void *context) {
    @try {
        imp(self, _cmd, observer, key_path, context);
        return NULL;
    } @catch (id exception) {
        // Retained while inside this @catch block, but that guarantee ends with
        // the block, and the caller inspects the exception after it returns.
        return objc_retain(exception);
    }
}

/// Rethrows an exception the guard decided is not its business.
///
/// The caller holds a retained reference; the throw consumes it like any other.
void gpui_macos_rethrow(id exception) {
    @throw exception;
}
