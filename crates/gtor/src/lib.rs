//! `gtor` allows you to write generators in Rust that are easy to write and easy to poll.
//!
//! # Features
//!
//! - Generators can yield values of any type, chosen per-generator via `yield_type`.
//! - Generators can take input arguments, just like ordinary functions.
//! - Generators can optionally return a final result value when they complete.
//! - All input arguments, yielded values, and return values can be passed by value or by reference.
//! - The borrow checker prevents any borrowing violations, just like in regular Rust code.
//! - Generators are stackless (they don't have their own stack that is suspended and restored).
//! - A generator can live on the stack (using [`pin!`](https://doc.rust-lang.org/stable/std/macro.pin.html)) or on the heap (using `Box::pin()`).
//!
//! ```
//! use gtor::{GeneratorItem, generator};
//! use std::pin::pin;
//!
//! #[generator(yield_type = usize)]
//! fn example_without_return() {
//!     for x in (1..10).rev() {
//!         if x > 5 {
//!             yield_value!(x);
//!         }
//!     }
//! }
//!
//! fn main() {
//!     let mut expected = [9, 8, 7, 6].into_iter();
//!     for val in pin!(example_without_return()) {
//!         assert_eq!(expected.next().unwrap(), val);
//!     }
//!     assert!(expected.next().is_none());
//! }
//! ```
//!
//! # How it works
//!
//! A `#[generator]`-annotated function is rewritten by the `gtor_macro` attribute macro into an
//! ordinary function that returns a [`Generator`]. The function body becomes an async block, and
//! each `yield_value!(x)` is rewritten into `ctx.yield_value(x).await`, where `ctx` is a
//! [`GeneratorContext`] passed into the body. A [`Generator`] is a [`Future`] under the hood, so
//! driving a generator is just polling that future; no separate stack is allocated or saved for
//! the generator's state.
//!
//! Depending on the declared return type, a generator is driven one of three ways:
//!
//! * No return type (or `-> ()`): it implements `Iterator<Item = Y>`.
//! * A return type `R`: it implements `Iterator<Item = GeneratorItem<Y, R>>`, and you can also
//!   use [`Generator::next_item`].
//! * `-> !`: it never completes and does not implement `Iterator`; drive it with
//!   [`Generator::next_value`].
//!
//! You can also build a generator without the macro via [`create_generator`] (see its docs for
//! an example).
#[cfg(test)]
mod test;

// Re-export for convenience
pub use gtor_macro::generator;

mod never;
pub use never::Never;

pub mod future;

use std::marker::PhantomData;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

/// Magic number embedded in the [`State`]. Used as a canary in debug builds only.
const STATE_MAGIC: u32 = 0x32561810;

/// The generator state.
struct State<Y> {
    #[cfg(debug_assertions)]
    magic_value: u32,
    yielded: Option<Y>,
}

impl<Y> State<Y> {
    /// Asserts the correctness of the embedded magic number.
    #[cfg(debug_assertions)]
    fn assert_magic_number(&self) {
        if self.magic_value != STATE_MAGIC {
            panic!(
                "Expected state magic value {STATE_MAGIC}, but found {}",
                self.magic_value
            );
        }
    }

    #[cfg(not(debug_assertions))]
    fn assert_magic_number(&self) {
        // Do nothing
    }
}

impl<Y> Default for State<Y> {
    fn default() -> Self {
        Self {
            #[cfg(debug_assertions)]
            magic_value: STATE_MAGIC,
            yielded: None,
        }
    }
}

/// The context inside a generator function.
///
/// This can be used to yield a value to the caller.
#[derive(Clone)]
pub struct GeneratorContext<Y> {
    phantom_data: PhantomData<fn() -> Y>,
}

impl<Y> GeneratorContext<Y> {
    fn new() -> Self {
        Self {
            phantom_data: PhantomData,
        }
    }

    /// Yields a value from the generator to the caller.
    ///
    /// Be sure to call `.await` on the resulting [`Future`].
    pub fn yield_value(&mut self, value: Y) -> impl Future<Output = ()> + '_ {
        Yield::new(value)
    }
}

/// A yield operation, containing the yielded value.
///
/// `Yield` contains a lifetime reference to the [`GeneratorContext`] that spawned it, preventing
///  multiple yields from existing at the same time (which would be a bug in the generator
/// function). Additionally, the lifetime reference prevents a `Yield` from escaping the generator
/// function.
struct Yield<'a, Y> {
    value: Option<Y>,
    phantom_data: PhantomData<&'a mut ()>,
}

impl<Y> Yield<'_, Y> {
    fn new(value: Y) -> Self {
        Self {
            value: Some(value),
            phantom_data: PhantomData,
        }
    }
}

impl<Y> Future for Yield<'_, Y> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: We're not moving `this` or any of its components around in memory.
        let this = unsafe { self.get_unchecked_mut() };
        match this.value.take() {
            None => Poll::Ready(()),
            Some(value) => {
                // SAFETY:
                // This depends on the the correct pointer being set in the executor (in this case
                // the `Generator::next()` implementation. `Yield` nor `GeneratorContext` can not be
                // constructed by the user. This means that a `Yield` can only appear inside a
                // `Generator`.
                //
                // Secondly, `Yield` can not escape its  encapsulating `Future` (async function)
                // because it is impossible to declare the correct return type:
                // - `GeneratorContext::yield_value()` does not name the concrete type.
                // - `Yield` is constructed with a lifetime tied to the `GeneratorContext` inside
                //    the `Future`. It is therefor impossible to specify a declare lifetime for the
                //    `Future<Output=Yield<'a, ...>>`.
                let state = unsafe {
                    match ctx.waker().data().cast::<State<Y>>().cast_mut().as_mut() {
                        None => unreachable!("BUG: The waker data pointer is not set."),
                        Some(x) => x,
                    }
                };

                // A bug-detection mechanism. It is only enabled in debug build.
                state.assert_magic_number();

                if state.yielded.replace(value).is_some() {
                    unreachable!("BUG: Yield future encountered an existing value in the state.");
                }

                Poll::Pending
            }
        }
    }
}

/// A [`Future`]-based generator that can yield any number of instances of `Y`.
///
/// A `Generator` is what a function annotated with the [`generator`] attribute macro turns into:
/// calling the function creates a `Generator` that, when driven,
/// hands back the values the function `yield_value!`s. It is stackless: it has no stack of its
/// own, and driving it is simply polling the `Future` it wraps.
///
/// Depending on the generator function's return type, `Y` is produced one of three ways:
///
/// * No return type (or `-> ()`): a `Generator` over a future whose output is `()` implements
///   `Iterator<Item = Y>`.
/// * A return type `R`: a `Generator` over a future whose output is `Return<R>` implements
///   `Iterator<Item = GeneratorItem<Y, R>>`.
/// * `-> !`: a `Generator` over a future whose output is [`Never`] never completes. It does not
///   implement `Iterator`; drive it with [`Generator::next_value`].
///
/// A `Generator` can live on the stack (using [`pin!`](std::pin::pin)) or on the heap (using
/// `Box::pin()`).
pub struct Generator<F, Y> {
    future: F,
    state: State<Y>,
    finished: bool,
}

impl<F, Y> Generator<F, Y> {
    fn new(future: F, state: State<Y>) -> Self {
        Self {
            future,
            state,
            finished: false,
        }
    }
}

/// An iterator item spawned from a generator that returns a result value.
///
/// This is the `Item` of the [`Iterator`] implemented by a generator whose function has a
/// return type: each yielded value is delivered as [`GeneratorItem::Yield`], and the final
/// result the function returns as [`GeneratorItem::Return`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum GeneratorItem<Y, R> {
    /// A value the generator yielded via `yield_value!`.
    Yield(Y),
    /// The final result the generator function returned when it completed.
    Return(R),
}

/// The final result value of a generator function.
///
/// This is normally not constructed directly: a generator function whose return type is `R`
/// produces a future whose output is `Return<R>` (which is how the [`Generator`] implements
/// `Iterator<Item = GeneratorItem<Y, R>>`). The `R` inside can be reached by
/// destructuring the `Return`.
#[must_use]
pub struct Return<R>(R);

impl<R> From<R> for Return<R> {
    fn from(value: R) -> Self {
        Self(value)
    }
}

/// Bridges a generator's future output type to the `Item` of its [`Iterator`] implementation.
///
/// A [`Generator`] is `Future`-based, but consumers drive it with `Iterator`. This trait
/// describes how the concrete output type of the wrapped future maps yields and completion to
/// iterator items:
///
/// * Output `()` (a generator with no final result): items are the yielded values `Y`
///   themselves, and completion produces nothing.
/// * Output [`Return<R>`] (a generator with a final result): items are
///   [`GeneratorItem<Y, R>`], with completion producing `GeneratorItem::Return`.
///
/// This is implemented for the output types the `#[generator]` macro can produce. It is not
/// expected to be implemented for user types.
pub trait IterableGenerator<Y> {
    /// The `Item` of the [`Iterator`] this output type corresponds to.
    type Item;

    /// Wraps a yielded value into an iterator item.
    fn create_yield(value: Y) -> Self::Item;

    /// Turns completion (the future's output) into an iterator item, if any.
    fn create_return(result: Self) -> Option<Self::Item>;
}

impl<Y> IterableGenerator<Y> for () {
    type Item = Y;

    fn create_yield(value: Y) -> Self::Item {
        value
    }

    fn create_return(_result: Self) -> Option<Self::Item> {
        None
    }
}

impl<Y, R> IterableGenerator<Y> for Return<R> {
    type Item = GeneratorItem<Y, R>;

    fn create_yield(value: Y) -> Self::Item {
        GeneratorItem::Yield(value)
    }

    fn create_return(result: Self) -> Option<Self::Item> {
        Some(GeneratorItem::Return(result.0))
    }
}

impl<F, Y, R> Generator<F, Y>
where
    F: Future<Output = R>,
{
    /// Drives the generator until it yields a value or completes, returning either.
    ///
    /// This is the low-level polling primitive. It polls the underlying future exactly once and
    /// reports what happened:
    ///
    /// * `GeneratorItem::Yield(y)` if the generator `yield_value!`d `y` before being suspended,
    /// * `GeneratorItem::Return(r)` if the generator function completed with result `r`.
    ///
    /// It panics in debug builds if the generator is polled after it has already completed, and
    /// at runtime if the future suspends on something other than a `yield_value!` (i.e. it
    /// `await`ed an unsupported future). For ordinary iteration, prefer the `Iterator`
    /// implementation or [`Generator::next_item`]/[`Generator::next_value`].
    pub fn poll_next(self: Pin<&mut Self>) -> GeneratorItem<Y, R> {
        debug_assert!(!self.finished, "Future was polled after it has finished.");

        // We're putting a pointer to the state on the waker. This pointer will be used by `Yield`
        // to set the yielded value directly in the state. This would be OK even if `self` were not
        // pinned here, since the pointer is only accessed inside the `Future::poll()` below and
        // we're not moving the generator around in memory during that time.
        let waker = generator_waker(&self.state);
        let mut cx = Context::from_waker(&waker);

        unsafe {
            // SAFETY:
            // - The future is immediately pinned again.
            // - The state is not moved in memory in this method.
            let this = self.get_unchecked_mut();
            match Pin::new_unchecked(&mut this.future).poll(&mut cx) {
                Poll::Pending => {
                    if let Some(value) = this.state.yielded.take() {
                        return GeneratorItem::Yield(value);
                    }

                    // We can't prevent future implementations from awaiting a foreign future (i.e.
                    // a future that does not belong to this crate) at compile-time. But we have to
                    // take into account that the user steps into this trap and detect such cases.
                    panic!(
                        "Underlying task is pending, but we have no yielded value. This means the generator implementation is awaiting an unsupported type of future."
                    );
                }
                Poll::Ready(value) => {
                    this.finished = true;
                    GeneratorItem::Return(value)
                }
            }
        }
    }
}

impl<F, Y> Generator<F, Y>
where
    F: Future,
    F::Output: IterableGenerator<Y>,
{
    fn iter_next(mut self: Pin<&mut Self>) -> Option<<F::Output as IterableGenerator<Y>>::Item> {
        if self.finished {
            return None;
        }

        match self.as_mut().poll_next() {
            GeneratorItem::Yield(yielded) => Some(F::Output::create_yield(yielded)),
            GeneratorItem::Return(result) => {
                // SAFETY: Nothing is moved in memory here.
                F::Output::create_return(result)
            }
        }
    }
}

impl<F, Y> Generator<F, Y>
where
    F: Future<Output = Never>,
{
    /// Returns the next value yielded by an infinite generator.
    ///
    /// This is available for generators declared `-> !`, whose output is [`Never`]. Because such
    /// a generator never completes, it does not implement `Iterator`; instead the caller drives
    /// it on demand by repeatedly calling this method.
    ///
    /// Panics if the generator somehow completes (which should be impossible, since its return
    /// type is `Never`).
    pub fn next_value(self: Pin<&mut Self>) -> Y {
        match self.poll_next() {
            GeneratorItem::Yield(yielded) => yielded,
            GeneratorItem::Return(_) => panic!("BUG: Future with return type Never somehow managed to complete."),
        }
    }
}

impl<F, Y, R> Generator<F, Y>
where
    F: Future<Output = Return<R>>,
{
    /// Returns the next [`GeneratorItem`] yielded by a generator that has a final result.
    ///
    /// This is available for generators whose output is [`Return<R>`]. It is a non-`Option`
    /// alternative to the `Iterator` implementation: each call returns either
    /// [`GeneratorItem::Yield`] or the terminal [`GeneratorItem::Return`], letting the caller
    /// match on both explicitly rather than unwrapping an `Option`.
    pub fn next_item(self: Pin<&mut Self>) -> GeneratorItem<Y, R> {
        match self.poll_next() {
            GeneratorItem::Yield(yielded) => GeneratorItem::Yield(yielded),
            GeneratorItem::Return(Return(result)) => GeneratorItem::Return(result),
        }
    }
}

impl<F, Y> Iterator for Pin<&'_ mut Generator<F, Y>>
where
    F: Future,
    F::Output: IterableGenerator<Y>,
{
    type Item = <F::Output as IterableGenerator<Y>>::Item;

    /// Returns the next yielded value, or `None` once the generator has completed.
    ///
    /// For a generator with no final result, `Item` is the yielded value `Y`. For a generator
    /// with a final result, `Item` is [`GeneratorItem<Y, R>`] and the final `Some` value is
    /// `GeneratorItem::Return`.
    fn next(&mut self) -> Option<Self::Item> {
        self.as_mut().iter_next()
    }
}

impl<F, Y> Iterator for Pin<Box<Generator<F, Y>>>
where
    F: Future,
    F::Output: IterableGenerator<Y>,
{
    type Item = <F::Output as IterableGenerator<Y>>::Item;

    /// Returns the next yielded value, or `None` once the generator has completed.
    fn next(&mut self) -> Option<Self::Item> {
        self.as_mut().iter_next()
    }
}

fn generator_waker<Y>(state: &State<Y>) -> Waker {
    unsafe fn clone(data_ptr: *const ()) -> RawWaker {
        RawWaker::new(data_ptr, &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

    let state_ptr = state as *const State<Y>;
    unsafe { Waker::from_raw(RawWaker::new(state_ptr.cast::<()>(), &VTABLE)) }
}

/// Creates a [`Generator`] from an async closure that yields through a [`GeneratorContext`].
///
/// This is the manual counterpart to the [`generator`] attribute macro. `future_factory` is
/// called once with a fresh [`GeneratorContext`] and returns a future; every
/// time that future `await`s a value produced by [`GeneratorContext::yield_value`], the resulting
/// generator suspends and hands that value back to the caller.
///
/// ```
/// use gtor::{create_generator, GeneratorContext};
/// use std::pin::pin;
///
/// fn main() {
///     let gen = create_generator(async move |mut ctx: GeneratorContext<usize>| {
///         let base = 12;
///         for i in 1..=3 {
///             ctx.yield_value(i * base).await;
///         }
///     });
///
///     let mut expected = vec![36, 24, 12];
///     let mut gen = pin!(gen);
///     for item in &mut gen {
///         assert_eq!(expected.pop().unwrap(), item);
///     }
///     assert_eq!(None, gen.as_mut().next());
/// }
/// ```
pub fn create_generator<F, Y>(future_factory: impl FnOnce(GeneratorContext<Y>) -> F) -> Generator<F, Y>
where
    F: Future,
{
    let state = State::default();
    let future = future_factory(GeneratorContext::new());
    Generator::new(future, state)
}

/// Creates a [`Generator`] from an async closure, applying a mapping to the resulting future.
///
/// This behaves like [`create_generator`], but the future returned by `future_factory` is first
/// transformed by `future_map` before being wrapped. This is what the [`generator`] macro uses to
/// adapt the future's output type (for example to wrap a final result in [`Return`]).
///
/// For most code, use `create_generator` directly or, better, the `#[generator]` macro.
pub fn create_generator_mapped<F, Y, F2>(
    future_factory: impl FnOnce(GeneratorContext<Y>) -> F,
    future_map: impl FnOnce(F) -> F2,
) -> Generator<F2, Y>
where
    F: Future,
{
    let state = State::default();
    let future = future_factory(GeneratorContext::new());
    Generator::new(future_map(future), state)
}
