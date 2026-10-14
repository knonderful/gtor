//! Internal helpers for adapting a generator's underlying future.
//!
//! The `#[generator]` macro wraps the function body in an async block and then applies one of
//! the functions in this module to that future, depending on the generator's declared return
//! type:
//!
//! * A return type of `()` (or none) leaves the future's `Output` as `()`.
//! * A return type of `!` uses [`map_to_never`], which keeps the `Never` output.
//! * Any other return type `R` uses [`map_to_return`], which wraps the output in
//!   [`Return<R>`] so the [`Generator`][crate::Generator] can expose it through
//!   [`GeneratorItem`][crate::GeneratorItem].
//!
//! These are re-exported for the macro's use; application code should not need them directly.
use crate::{Never, Return};
use core::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

struct MapToReturn<F> {
    future: F,
}

impl<F, R> Future for MapToReturn<F>
where
    F: Future<Output = R>,
{
    type Output = Return<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We're not moving the future around in memory.
        let fut = unsafe { self.map_unchecked_mut(|this| &mut this.future) };
        fut.poll(cx).map(|r| Return(r))
    }
}

/// Wraps a future's output `R` in [`Return<R>`].
///
/// Used by the `#[generator]` macro so that a generator function with a concrete return type
/// yields a `Generator` whose iterator items are [`GeneratorItem`][crate::GeneratorItem].
pub fn map_to_return<R>(future: impl Future<Output = R>) -> impl Future<Output = Return<R>> {
    MapToReturn { future }
}

/// Returns a future whose output is already [`Never`], unchanged.
///
/// Used by the `#[generator]` macro for infinite generators (declared `-> !`); the mapping is
/// symmetric with [`map_to_return`] but performs no transformation.
pub fn map_to_never(future: impl Future<Output = Never>) -> impl Future<Output = Never> {
    future
}
