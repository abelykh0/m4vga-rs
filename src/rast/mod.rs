pub mod bitmap_1;

use core::sync::atomic::{Ordering, AtomicUsize, AtomicBool};
use core::cell::Cell;
use scopeguard::defer;
use super::Pixel;

pub const TARGET_BUFFER_SIZE: usize = super::MAX_PIXELS_PER_LINE + 32;
pub type TargetBuffer = [Pixel; TARGET_BUFFER_SIZE];

pub struct RasterCtx {
    /// Number of AHB cycles per pixel of output. Provided by the driver based
    /// on the current mode; raster callbacks can adjust to derive new modes.
    /// Note: values below 4 are undefined.
    pub cycles_per_pixel: usize,
    /// Number of times to repeat this line after its first appearance -- zero
    /// gives full vertical resolution, while larger numbers divide vertical
    /// resolution by (1+n). Provided by the driver based on the current mode;
    /// raster callbacks can adjust to derive new modes.
    ///
    /// Setting `repeat_lines` to a non-zero value skips calling the raster
    /// callback for that many lines, which can be used to save compute.
    pub repeat_lines: usize,
    /// Rasterization range within `target`. The range is *empty* when the
    /// callback starts! To show any actual video, the callback *must* replace
    /// it with the range of valid pixels in `target`.
    ///
    /// If you set this outside of the bounds of `target`, the driver's behavior
    /// is undefined. (Not unsafe -- it just reserves the right to replace video
    /// output with an embarrassing picture of you.)
    pub target_range: core::ops::Range<usize>,
}

pub fn solid_color_fill(_line_number: usize,
                        target: &mut TargetBuffer,
                        ctx: &mut RasterCtx,
                        width: usize,
                        fill: Pixel) {
    target[0] = fill;               // Same color.
    ctx.target_range = 0..1;            // One pixel.
    ctx.cycles_per_pixel *= width;      // Stretched across the whole line.
    ctx.repeat_lines = 1000;            // And don't ask again.
}

// ---

const EMPTY: usize = 0;
const LOADING: usize = 1;
const LOADED: usize = 2;
const LOCKED: usize = 3;

/// A mechanism for loaning a reference to an interrupt handler (or another
/// thread).
///
/// An `IRef` is initially empty. An exclusive reference to some data can be
/// *donated* by using the `donate` method; this puts the `IRef` into the
/// *loaded* state, runs a supplied closure, and then returns it to *empty*
/// before returning.
///
/// The contents of the `IRef` can be observed using the `observe` method. If
/// the `IRef` is *loaded*, `observe` switches it to *locked* state and runs a
/// closure on a reference to the contents. When the closure finishes, the
/// `IRef` returns to *loaded*.
///
/// `donate` is intended primarily for non-interrupt code, and can busy-wait.
/// `observe` cannot, and is safer for use by interrupts. See each method's
/// documentation for specifics.
#[derive(Debug)]
pub struct IRef {
    state: AtomicUsize,
    poisoned: AtomicBool,
    contents: Cell<(usize, usize)>,
}

unsafe impl Sync for IRef {}

impl IRef {
    /// Creates an `IRef` in the *empty* state.
    ///
    /// ```ignore
    /// static REF: IRef<MyType> = IRef::new();
    /// ```
    pub const fn new() -> Self {
        IRef {
            state: AtomicUsize::new(EMPTY),
            poisoned: AtomicBool::new(false),
            contents: Cell::new((0, 0)),
        }
    }

    /// Donates an exclusive reference `val` to observers of the `IRef` for the
    /// duration of execution of `scope`.
    ///
    /// When `scope` returns, `donate` will busy-wait until any observer of the
    /// `IRef` is done, and then atomically reset the `IRef` to empty, ensuring
    /// that the caller regains exclusive use of `val`.
    ///
    /// # Panics
    ///
    /// If `self` is not empty. This means `donate` cannot be called recursively
    /// or from multiple threads simultaneously.
    pub fn donate<'env, F, R>(&self,
                              val: &'env mut F,
                              scope: impl FnOnce() -> R)
        -> R
    where F: for<'isr> FnMut(usize,
                             &'isr mut TargetBuffer,
                             &'isr mut RasterCtx) + Send + 'env,
    {
        let r = self.state.compare_exchange(
            EMPTY,
            LOADING,
            Ordering::Acquire,
            Ordering::Relaxed,
        );
        assert_eq!(r, Ok(EMPTY), "concurrent/reentrant donation to IRef");

        let val: &mut (dyn FnMut(_, _, _) + Send + 'env) = val;
        let val: (usize, usize) = unsafe { core::mem::transmute(val) };

        // By placing the cell in LOADING state we now have exclusive control.
        // In particular, it is safe to do this:
        self.contents.set(val);
        self.state.store(LOADED, Ordering::Release);

        defer! {{
            // Busy-wait on the interrupt.
            loop {
                let r = self.state.compare_exchange_weak(
                    LOADED,
                    EMPTY,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                    );
                if let Ok(_) = r { break }
                // TODO: this would be a polite place to WFI. Note that core's
                // spin_loop_hint does not currently produce WFI.
            }

            if self.poisoned.load(Ordering::Acquire) {
                panic!("IRef poisoned by panic in observer")
            }
        }}

        scope()
    }

    /// Locks the `IRef` and observes its contents, if it is not empty or
    /// already locked.
    ///
    /// If this is called concurrently with `supply`, it will execute `body`
    /// with the reference donated by the caller of `supply`. During the
    /// execution of `body`, the `IRef` will be *locked*, preventing both
    /// other concurrent/reentrant calls to `observe` from succeeding, and the
    /// caller of `supply` from returning.
    ///
    /// If the `IRef` is either empty or locked, returns `None` without
    /// executing `body`.
    ///
    /// This operation will never busy-wait (unless, of course, `body` contains
    /// code that will busy-wait).
    ///
    /// # Note
    ///
    /// The `body` closure must be valid for any possible reference lifetime,
    /// because it is not permitted to make assumptions about how long the
    /// donated reference lives. In particular, the donated reference is *not*
    /// `'static` even if `self` is, because that would allow it to leak.
    /// Omitting the `for<'a>` silently allows this bug by inferring the
    /// lifetime of `self` -- do not be tempted.
    pub fn observe<R, F>(&self,
                         body: F)
        -> Option<R>
    where F: for<'e> FnOnce(&'e mut (dyn for<'r> FnMut(usize,
                                                       &'r mut TargetBuffer,
                                                       &'r mut RasterCtx) + Send))
             -> R
    {
        self.state
            .compare_exchange(
                LOADED,
                LOCKED,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .ok()
            // Having successfully exchanged LOADED for LOCKED, we know no other
            // thread is going to try to muck with the cell. Time to access its
            // contents. This is safe because of the atomic exchange above.
            .map(|_| {
                if self.poisoned.load(Ordering::Acquire) {
                    panic!("IRef poisoned by panic in observer")
                }

                let poisoner = scopeguard::guard((),
                    |_| self.poisoned.store(true, Ordering::Release));

                let result = {
                    let r = self.contents.get();
                    // We do *not* know the correct lifetime for the &mut.  This
                    // is why the `body` closure is `for<'a>`.
                    let r: &mut (dyn for<'r> FnMut(_, &'r mut _, &'r mut _)
                                 + Send) =
                        unsafe { core::mem::transmute(r) };
                    body(r)
                };
                self.state.store(LOADED, Ordering::Release);
                scopeguard::ScopeGuard::into_inner(poisoner);
                result
            })
    }

}
