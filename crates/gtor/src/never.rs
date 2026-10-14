/// A marker type indicating a generator function that never returns.
///
/// This type is not constructible (its single field is a private, uninhabited unit tuple), so no
/// code can actually produce a value of it. As a result, a function whose generator output is
/// `Never` must never complete, making it suitable for modeling an infinite generator.
///
/// You write this as the return type of a `#[generator]` function by using `-> !`; the
/// `generator` macro translates `!` to `Never` in the generated future's `Output`. Such a
/// generator does not implement `Iterator`; drive it with
/// [`Generator::next_value`][crate::Generator::next_value].
pub struct Never(());
