/*! Module for Keybow 12-key illuminated keypad.
 *
 * Keybow is a 12-key keypad with RGB LEDs under each key, that mounts
 * onto a Rapsberry Pi Zero. Or, probably better described as the very
 * small Pi Zero mounts onto it.
 *
 * The LEDs are set over SPI and the keys are read over GPIO.
 *
 * The Keybow is no longer manufactured. For more info:
 * <https://shop.pimoroni.com/en-us/products/keybow>
 *
 * # This module supports writing to the LEDs, and reading from the keys.
 *
 * Usable from either async or conventional blocking code (both at the
 * same time, if you that might be useful).
 *
 * Note: Functions for setting LEDs are a two-part thing.
 *
 * - First, set the desired color in the "off-screen frame buffer",
 *   and
 *
 * - Second, updating the physical LEDs to reflect this off-screen
 *   data.
 *
 */

mod cust_reg;
mod cust_reg_async;
pub mod hw_specific;
mod keys;
mod leds;
mod macros;

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::{error::Error, fmt::Debug};

use crossbeam_channel;

use thiserror::Error;

use rppal::gpio::{Gpio, InputPin, Level, Trigger};
use rppal::spi;

#[derive(Error, Debug)]
pub enum KError {
    #[error(
        "Bad LED location {bad_location:?}, (expected < hw_specific::NUM_LEDS, which is {})",
        hw_specific::NUM_LEDS
    )]
    BadKeyLocation { bad_location: KeyLocation },
    #[error(
        "Brightness is a global multiplier applied to all LEDs, must be 0.0 - 1.0 inclusive, not {}",
        bad_brightness,
    )]
    BadBrightness { bad_brightness: f32 },
}

#[derive(Debug, Clone)]
/// The struct for interacting with Keybow.
///
/// This single piece of hardware can be talked to by multiple client
/// threads, as they please, with no coordination needed between them,
/// at least regarding things like deadlocks. Certainly any business
/// logic must be coordinated. (What are the keys and LEDs being used
/// for?)
///
/// A single instance of this struct is created by the first call of
/// `new()`, and subsequent new()s will get a clone of the struct.
///
/// All Those Mutexes
/// -----------------
///
/// Because this struct can be shared between multiple threads, we
/// need all those top-level Mutexes to let them all share it.
///
/// As for those Vecs of Mutexes, the rppal gpio library uses a thread
/// for each input line (for each hardware key) and Mutexes are needed
/// to protect the data they need to access.
///
/// Clone Size
/// ----------
///
/// The fixed size of this struct is, small, a couple dozen bytes.
///
/// The size of the three fields that pertain to input keys are
/// incrementally small, a function of the number of hardware input
/// keys.
pub struct Keybow {
    /// Mutexes allow access by multiple client threads.
    led_data: Arc<Mutex<[rgb::RGB<u8>; hw_specific::NUM_LEDS]>>,
    spi: Arc<Mutex<spi::Spi>>,
    gpio_keys: Vec<Arc<Mutex<rppal::gpio::InputPin>>>,

    /// Mutexes allow each key to be debounced by a pair of threads,
    /// one is started by rppal for recording key activity, and a
    /// second one started by us for detecting when activity has
    /// stopped.
    key_state_debounce_data: Vec<Arc<Mutex<KeyStateDebounceData>>>,

    debounce_thread_vec: Vec<Arc<thread::JoinHandle<()>>>,

    /// What key events clients are interested in. Length of Vec is
    /// number of
    /// `register_events_blocking()`/`register_events_asyc()` values
    /// outstanding.
    ///
    /// Our client has an opaque copy of this and is free to delete at
    /// as will. We need our own copy, and it can't vanish while me
    /// might be looking at it, so we keep a weak reference. We will
    /// delete our copy if when we process the next key event we find
    /// we can't `upgrade()` the weak reference to real data.
    cust_reg: Arc<Mutex<Vec<Weak<Mutex<CustRegInner>>>>>,

    /// `RwLock` needed so `debounce_thread()`s (one per input key)
    /// can share for setting current values. Though a single lock is
    /// shared across as many threads as there are keys, this gets
    /// written on a multiple ms scale, so there should be no
    /// contention problems.
    current_up_down_values: Arc<RwLock<[KeyPosition; hw_specific::NUM_KEYS]>>,

    /// We don't need to access this ourselves, but I think it needs
    /// to exist somewhere. It is an Arc, and therefore cheap to
    /// clone.
    _gpio: Gpio,

    /// A global multiplier that applies to all the LEDs when
    /// `show_leds()` is called.
    brightness: Arc<Mutex<f32>>,
}

#[derive(Debug, Clone, Copy)]
/// One of these gets returned for every debounced key event.
pub struct KeyEvent {
    /// Which key, by index position.
    pub key_index: usize,

    /// Which key, by `char` assigned to the key.
    pub char: char,

    /// Whether the key is up or down.
    pub key_position: KeyPosition,

    /// When the event happened, as an `Instant`.
    pub event_instant: Instant,

    /// When the event happened, as a `SystemTime`.
    pub event_systime: SystemTime,

    /// All the key values at the time of the key event. Useful for
    /// metakeys.
    pub up_down_values: [KeyPosition; hw_specific::NUM_KEYS],

    /// Incrementing event serial number. Note this number is
    /// incremented with each key event queued, evenn though it is not
    /// part of the return values from `get_next_char()` nor
    /// `wait_next_char()`.
    pub event_num: usize,

    /// Number of events currently queued.
    pub additional_events_still_queued: usize,
}

#[derive(Debug, Clone)]
struct CustRegInner {
    /// We are interested in key down events from these keys.
    keydown_mask: [bool; hw_specific::NUM_KEYS],

    /// We are interested in key up events from these keys.
    keyup_mask: [bool; hw_specific::NUM_KEYS],

    /// Where events we are asked for will be stored. Need a Mutex so
    /// thread that puts events in the queue doesn't interfere with
    /// thread that reads them.
    ///
    /// New events are added to the end with `push_back()`, so lowest
    /// indexes are the oldest events.
    event_queue: Arc<Mutex<VecDeque<KeyEvent>>>,

    /// Used to wake blocked async subscribers.
    async_sender: tokio::sync::watch::Sender<()>,
    async_receiver: tokio::sync::watch::Receiver<()>,

    /// Used to wake blocked synchronous subscribers.
    sync_sender: crossbeam_channel::Sender<usize>,
    sync_receiver: crossbeam_channel::Receiver<usize>,

    /// Characters to assign to each key.
    key_mapping: [char; hw_specific::NUM_KEYS],

    /// Number of events queued so far.
    event_count: usize,
}

#[derive(Debug, Clone)]
/// Struct for getting debounced key events via synchronous (blocking)
/// methods. To obtain one of these call
/// `keybow::register_events_blocking()`. This can also be used as an
/// iterator. See: [`CustReg::next()`].
///
/// If you want an async (Tokio) version call
/// `register_events_async()` instead.
pub struct CustReg {
    /// We need our client code to have a mutex to the registration
    /// data so we can keep a weak reference to it, we can know when
    /// it has gone out of scope and then delete our internal copy.
    ///
    /// But we want all of this internal stuff to be opaque to the
    /// client code; the client code should just make simple method
    /// calls. So it is a simple struct on the outside. On the inside?
    /// The mutex we actually want the client to have.
    reg_inner: Arc<Mutex<CustRegInner>>,

    /// Timeout used when keys are read from `CustReg` as in
    /// iterator. This value can be changed while running.
    pub iter_timeout: Duration,

    /// Size of event queue. (Changing this value has no effect, the
    /// queue was already created.)
    pub queue_capacity: usize,
}

#[derive(Debug, Clone)]
/// Struct for getting debounced key events via async (Tokio)
/// methods. To obtain one of these call
/// `keybow::register_events_async()`. This can also be used as an
/// iterator. See: [`CustRegAsync::next()`].
///
/// If you want a synchronous (blocking) version call
/// `register_events_blocking()` instead.
pub struct CustRegAsync {
    /// This uses `Custreg` under the hood.
    pub real_cust_reg: CustReg,

    /// Timeout used when keys are read from `CustReg` as in
    /// iterator. This value can be changed while running.
    pub iter_timeout: Duration,

    /// Size of event queue. (Changing this value has no effect, the
    /// queue was already created.)
    pub queue_capacity: usize,
}

#[derive(Debug, Clone)]
/// Used by `debounce_thread()` (that we started) and
/// `debounce_callback()` (called by thread rppal started) to
/// communicate with each other.
struct KeyStateDebounceData {
    key_index: usize,
    debounce_state: DebounceState,
}

#[derive(Debug, Clone)]
enum DebounceState {
    /// All is quiet, debounce thread is parked.
    Stable,

    /// Debounce thread is sleeping, then checking, then sleeping…
    Unstable(UnstableState),
}

#[derive(Debug, Copy, Clone)]
/// There is one of these for each key, `debounce_callback()` writes
/// this struct everytime a raw key event causes it to be called;
/// `debounce_thread()` reads from it.
struct UnstableState {
    /// Key read as up or pressed down?
    raw_key_position: KeyPosition,

    /// The is a time in the future. Everytime the callback is called
    /// this time gets pushed out further into the future; as long as
    /// this value keeps changing, `debounce_thread()` keeps
    /// sleeping. Until finally the sleep time quits changing,
    /// indicating the key is no longer bouncing.
    test_stable_time: Instant,
}

#[derive(Debug, Copy, Clone)]
pub struct Point {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug)]
pub enum KeyLocation {
    Coordinate(Point),
    Index(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyPosition {
    /// Key is not being pressed.
    Up,

    /// Key is being pressed down.
    Down,
}

/// Look up (x,y) and get back key/LED index.
fn coord_to_index(coordinate: Point) -> Result<usize, KError> {
    let two_d_array = HardwareInfo::get_info().xy_to_index_lookup;

    if coordinate.x >= two_d_array.len() || coordinate.y >= two_d_array[0].len() {
        Err(KError::BadKeyLocation {
            bad_location: KeyLocation::Coordinate(coordinate),
        })
    } else {
        Ok(two_d_array[coordinate.x][coordinate.y])
    }
}

#[derive(Debug)]
pub struct HardwareInfo {
    /// Name of hardware model.
    pub name: &'static str,

    /// Number of keys in keypad.
    pub num_keys: usize,

    /// Number of LEDs in keypad (probably always the same as `num_keys`).
    pub num_leds: usize,

    /// Keys index (array index) to GPIO pin (array value).
    pub key_index_to_gpio: [u8; hw_specific::NUM_KEYS],

    /// 2D array to map from (X,Y) to key/LED index.
    pub xy_to_index_lookup: [[usize; 3]; 4],

    /// Sequence of LED indexes to send to SPI.
    pub spi_seq_to_index_leds: [usize; hw_specific::NUM_LEDS],

    /// Human-readable description of hardware.
    pub layout_text: String,
}

/// An alternative to calling a generic `.unwrap()`, this is something
/// that can't accidentally be applied to an `Option` or the wrong
/// kind of `Result`.
///
///    `some_mutex.lock().unwrap_or_else(mutex_poison);`
///
/// This will only work against a `MutexGuard`.
#[allow(clippy::needless_pass_by_value)]
fn mutex_poison<T>(g: PoisonError<MutexGuard<T>>) -> MutexGuard<'_, T> {
    panic!("mutex poisoned {g:?}")
}

/// An alternative to calling a generic `.unwrap()`, this is something
/// that can't accidentally be applied to an `Option` or the wrong
/// kind of `Result`.
///
///    `some_mutex.lock().unwrap_or_else(write_poison);`
///
/// This will only work against a `RwLockWriteGuard`.
#[allow(clippy::needless_pass_by_value)]
fn write_poison<T>(g: PoisonError<RwLockWriteGuard<T>>) -> RwLockWriteGuard<'_, T> {
    panic!("mutex poisoned {g:?}")
}

/// An alternative to calling a generic `.unwrap()`, this is something
/// that can't accidentally be applied to an `Option` or the wrong
/// kind of `Result`.
///
///    `some_mutex.lock().unwrap_or_else(read_poison);`
///
/// This will only work against a `RwLockReadGuard`.
#[allow(clippy::needless_pass_by_value)]
fn read_poison<T>(g: PoisonError<RwLockReadGuard<T>>) -> RwLockReadGuard<'_, T> {
    panic!("mutex poisoned {g:?}")
}

/// An alternative to calling a generic `.unwrap()`, this is something
/// that can't accidentally be applied to an `Option` or the wrong
/// kind of `Result`.
///
///    `some_mutex.lock().unwrap_or_else(try_send_error);`
///
/// This will only work against a `crossbeam_channel::TrySendError`.
#[allow(clippy::needless_pass_by_value)]
fn try_send_error<T>(g: crossbeam_channel::TrySendError<T>) {
    panic!("crossbeam channel is full {g:?}")
}
