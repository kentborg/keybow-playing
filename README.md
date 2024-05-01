# Work in Progress

This is a work-in-progress. Its goals are fuzzy because the real
purpose is to be educational; I have been studying Rust for years, but
I have never put in the time programming in Rust to get good at it.

I am fixing that now, and it is working. (And Rust is proving very nice.)

# The Problem

A few years ago I bought, and promptly ignored, a
[Keybow](https://shop.pimoroni.com/products/keybow). It is a keypad
that has 12-push bottons, with an RGB LED under each translucent
keycap. (Alas Keybow is no longer made, so this repository is not
likely to be of much use to others.)

It connects to a Raspberry Pi Zero: a little board that costs about
$15, runs Linux, boots off a micro SD card, has 512MB RAM, and a not
very powerful single CPU core. No powerhouse, but ample for me,
because I am programming in Rust and not Python.

Time to finally play with my Keybow. In Rust.

# First Step, a `keybow` Module

I have some incomplete ideas about what this might eventually do, but
first I need a library to access the hardware. As this is a
discontinued product, my code is not something I expect to put on
`crates.io`, so I might as well make my `keybow` module part of
whatever my larger binary program this turns into. It makes my life
simpler now and I can still pull it out later if I need to.

## Lay of the Land

- 12-LEDs are set over SPI.
- 12-keys are read over GPIO pins.
- This is Raspberry Pi hardware.

The Raspberry Pi-specific crate
[rppal](https://docs.rs/rppal/latest/rppal/) does GPIO and SPI and
seems to have a good reputation. After I got some valuable answers on
how to use its GPIO module, I decided to go with it.

## My `Keybow` Requirements

As I said, my requirements are vague, and maybe I'm being too
featureful, but my purpose is to learn:

- Set LED colors. (That was very easy.)

- Read keys. (Easy, but not very useful.)

- Read *debounced* keys. (Useful but not as easy…here the fun
  begins.)

- Queue debounced key presses and releases as events.

- Allow client code to register for which events it is interested in
  by two mask values (one mask of which keys for down events, another
  mask of which keys for up events).

- Client code can ask for next event (optionally next event that
  matches more narrow masks).

- Client code can block on getting next event, with a timeout.

- Arbitrary number of theads can be clients, they can spawn and exit
  as they please, in whatever sequence, each getting whatever events
  it registered interest in.

- Be fast and not a pig.


## Background: Debouncing

When a mechanical switch makes contact it will "bounce". That is, make
contact, then unmake contact, then make contact again, etc. Possibly
very many times before settling down to being "on". Similarly, when
the switch is released, it does the whole thing again, until the
switch finally settles down to again being "off".

## Lot of Threads

There is no debouncing implemented in Raspberry Pi hardware, so I
needed implement my own, to turn lots of raw key events into clean
debounced values.

The way to find out about raw GPIO events using the `rppal` crate is
to register a callback function for each GPIO line—so one callback for
each Keybow key. Looking under the hood, in `gdb` (so nice to be back
after struggling with `pdb` for so long), I see `rppal` creates
12-threads. Each calls the function I supplied when there is a raw
event. These functions get called a lot when the keys are touched.

That was step one, I have the raw data. How to debounce that? How
about a dozen new debounce threads! Each looking at the timing of its
corresponding debounce callback. My threads spend most of their time
either `park()`ed or sleeping, consuming relatively few resouces, and
exhibiting good latencies, so this doesn't seem excessive.

I do this:

- The debounce callback function notes the event data, and notes a
  time (`Instant`) somewhat into the future at which point the key
  might have settled down. (Assuming no new raw events have happened
  before then.)

- If this is start of a new event, the debounce callback `unpark()`s
  the corresponding debounce thread.

- The debounce callback then returns.

- Now the freshly unparked debounce thread looks at the stored
  `Instant` and sleeps until then.

- When the debounce thread wakes up, it checks to see whether the
  stored future `Instant` changed while it was sleeping. If it has
  that means there is still activity, so it sleeps again until the new
  `Instant` value.

- If there has been no activity while the debounce thread was
  sleeping, then we are stable. Record the details, and `park()` until
  the next event.

Yes, I have created two-dozen threads, but they are small and they
mostly don't do much. It seems the Pi Zero can handle them with ease.


## Async

The resulting code works as both sync or async code, and the code to
use the two flavors is only as different as I think is necessary. And
both sync and async could be used at the same time, if that were
somehow useful.


## Learning a Lot

I honestly don't know what I think of my approach so far, but I am
pleased it works and that Rust is willing to do what I want it to
do. I suspect I have too many mutexes.

Were I to do this again from scratch I wonder how similar the result
would be. Maybe my debounce threads would instead be in async/await
land. I looked at little into calling an async `Waker` from an OS
thread, but that was getting too specialized for me at that point.

I have been impressed with how nice the Rust compiler is as a guide to
what code I need to touch whenever I take working code and start
making changes to add some new aspect. Once the the compiler is happy,
I copy the code to the target, build it again there, and usually it
works correctly first try.

# Next Steps

Do something in application space.

# Examples

At the moment `src/main.rs` is a fairly minimal example of how to use
my `Keybow` module.
