# `gtor`

`gtor` allows you to write generators in Rust that are easy to write and easy to poll.

## About

`gtor` gives you a small, `async`/`await`-based generator mechanism for Rust. You write an ordinary-looking function, annotate it with `#[generator(yield_type = ...)]`, and call `yield_value!(...)` from anywhere in its body. The macro turns the function into a `Generator` — a `Future`-backed value that hands you yielded values one at a time, exactly when you poll it.

```rust
use gtor::generator;
use std::pin::pin;

#[generator(yield_type = usize)]
fn example_without_return() {
    for x in (1..10).rev() {
        if x > 5 {
            yield_value!(x);
        }
    }
}

fn main() {
    for val in pin!(example_without_return()) {
        println!("{val}");
    }
    // prints: 9, 8, 7, 6
}
```

## Table of Contents

- [About](#about)
- [Features](#features)
- [Installing](#installing)
- [Example](#example)
- [Using](#using)
  - [Finite generators](#finite-generators)
  - [Generators with a return value](#generators-with-a-return-value)
  - [Infinite generators](#infinite-generators)
  - [Generators with references and lifetimes](#generators-with-references-and-lifetimes)
- [How it works](#how-it-works)
- [MSRV](#msrv)
- [License](#license)

## Features

- Generators can yield values of any type, chosen per-generator via `yield_type`.
- Generators can take input arguments, just like ordinary functions.
- Generators can optionally return a final result value when they complete.
- All input arguments, yielded values, and return values can be passed by value or by reference.
  - The borrow checker prevents any borrowing violations, just like in regular Rust code.
- Generators are stackless (they don't have their own stack that is suspended and restored).
- A generator can live on the stack (using [`pin!`](https://doc.rust-lang.org/stable/std/macro.pin.html)) or on the heap (using `Box::pin()`).

## Installing

```console
$ cargo add gtor
```

## Example

A generator that yields `usize` values and has no final result:

```rust
use gtor::generator;
use std::pin::pin;

#[generator(yield_type = usize)]
fn count_down() {
    for x in (1..10).rev() {
        if x > 5 {
            yield_value!(x);
        }
    }
}

fn main() {
    for val in pin!(count_down()) {
        println!("{val}");
    }
}
```

A more complete tour of the different shapes of generators lives in [`examples/gtor-macro-example`](examples/gtor-macro-example). You can run it with:

```console
$ cargo run -p generator-macro-example
```

## Using

### Finite generators

A generator whose body ends (i.e. with no explicit return type, or an explicit `-> ()`) implements [`Iterator`](https://doc.rust-lang.org/stable/std/iter/trait.Iterator.html) over its yielded values, so it can be used directly in a `for` loop:

```rust
use gtor::generator;
use std::pin::pin;

#[generator(yield_type = usize)]
fn example_without_return() {
    for x in (1..10).rev() {
        if x > 5 {
            yield_value!(x);
        }
    }
}

fn main() {
    let mut expected = [9, 8, 7, 6].into_iter();
    for val in pin!(example_without_return()) {
        assert_eq!(expected.next().unwrap(), val);
    }
    assert!(expected.next().is_none());
}
```

### Generators with a return value

If the generator function has a return type (other than `()` or `!`), it can hand back a final result when it completes. The `Iterator` then yields [`GeneratorItem::Yield(y)`] for every yielded value and a final [`GeneratorItem::Return(r)`]:

```rust
use gtor::{generator, GeneratorItem};
use std::pin::pin;

#[generator(yield_type = usize)]
fn example_with_return() -> String {
    for x in (1..10).rev() {
        if x > 5 {
            yield_value!(x);
        }
    }

    "done".to_string()
}

fn main() {
    for item in pin!(example_with_return()) {
        match item {
            GeneratorItem::Yield(y) => println!("Yield({y})"),
            GeneratorItem::Return(r) => println!("Return({r})"),
        }
    }
}
```

### Infinite generators

A generator declared `-> !` never completes. Since `Iterator::next()` implies the stream ends at some point, this kind of generator does not implement `Iterator`. Instead, poll it explicitly with `Generator::next_value()`, which returns the next yielded value directly:

```rust
use gtor::generator;
use std::pin::pin;

#[generator(yield_type = usize)]
fn example_with_infinity() -> ! {
    let mut i = 261722;
    loop {
        yield_value!(i);
        i %= 1000000; // avoid overflow
        i = i * 2 - i / 3;
    }
}

fn main() {
    let mut gen = pin!(example_with_infinity());

    for _ in 0..3 {
        let val = gen.as_mut().next_value();
        println!("{val}");
    }
}
```

This shape is intended for generators that are pulled on demand, such as ID generators, rather than being driven by a closed loop.

### Generators with references and lifetimes

Generators take arguments and yield/return references with ordinary lifetime rules. The macro attaches the relevant lifetimes to the generated `Future`, so the borrow checker can verify the whole thing:

```rust
use gtor::{generator, GeneratorItem};
use std::pin::pin;

#[derive(Debug)]
struct Person {
    name: String,
    friends: Vec<Person>,
}

impl From<&str> for Person {
    fn from(value: &str) -> Self {
        Self {
            name: value.to_string(),
            friends: vec![],
        }
    }
}

impl Person {
    fn add_friend(&mut self, name: &str) {
        self.friends.push(name.into());
    }

    #[generator(yield_type = &Person)]
    fn friends<'a>(&self, prefix: &'a str) -> &str {
        for f in &self.friends {
            if f.name.starts_with(prefix) {
                yield_value!(&f);
            }
        }

        self.name.as_str()
    }
}

fn main() {
    let mut hank = Person::from("Hank");
    hank.add_friend("Bob");
    hank.add_friend("Jimmy");
    hank.add_friend("Beatrice");

    for item in pin!(hank.friends("B")) {
        match item {
            GeneratorItem::Yield(friend) => println!("Yield({friend:?})"),
            GeneratorItem::Return(me) => println!("Return({me:?})"),
        }

        // hank.add_friend("Yono");
        // ^^^^
        // This would not compile: `hank`'s lifetime is bound to the future, so as
        // long as we hold a reference to the generator we cannot modify `hank`.
    }
}
```

## How it works

- A `#[generator]`-annotated function is rewritten by the `gtor-macro` attribute macro into an ordinary function that returns a `Generator`.
- The function body becomes an async block: each `yield_value!(x)` is rewritten into `ctx.yield_value(x).await`, where `ctx` is a `GeneratorContext` passed into the body.
- A `Generator` is a [`Future`](https://doc.rust-lang.org/stable/std/future/trait.Future.html) under the hood, so driving a generator is just polling that future. No separate stack is allocated or saved for the generator's state.
- Depending on the declared return type, the generator implements one of:
  - `Iterator<Item = Y>` — for a `()` (or no) return type,
  - `Iterator<Item = GeneratorItem<Y, R>>` — for a concrete return type `R`,
  - no `Iterator` impl — for `!`, where you drive it with `next_value()` instead.

## MSRV

This crate requires rustc 1.85.0 or later.

## License

`gtor` is licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
