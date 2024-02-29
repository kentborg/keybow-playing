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
 * Note: Functions for setting LEDs are a two-part thing.
 *
 * - First, set the desired color in the "off-screen frame buffer",
 *   and
 *
 * - Second, updating the physical LEDs to reflect this off-screen
 *   data.
 *
 * Apologies for pretending a humble illuminated key pad as a fancy
 * mega-pixel display.
 *
 *
 */

pub mod hw_specific;
mod keys;
mod leds;
pub mod macros;

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::{error::Error, fmt::Debug};

use bitmaps::Bitmap;
use thiserror::Error;

use rppal::gpio::{Gpio, InputPin, Level, Trigger};
use rppal::spi;

#[derive(Error, Debug)]
pub enum KeybowError {
    #[error(
        "Bad LED location {bad_location:?}, (expected < hw_specific::NUM_LEDS, which is {})",
        hw_specific::NUM_LEDS
    )]
    BadKeyLocation { bad_location: KeyLocation },
}

#[derive(Debug, Clone)]
/// The struct for interacting with Keybow.
pub struct Keybow {
    led_data: Arc<Mutex<[rgb::RGB<u8>; hw_specific::NUM_LEDS]>>,
    brightness: f32,
    spi: Arc<Mutex<spi::Spi>>,
    _gpio: Gpio,
    gpio_keys: Vec<Arc<Mutex<rppal::gpio::InputPin>>>,
    key_state_debounce_data: Vec<Arc<Mutex<KeyStateDebounceData>>>,
    debounce_thread_vec: Vec<Arc<Mutex<thread::JoinHandle<()>>>>,
    debounce_threshold: Duration,
    cust_reg: Arc<Mutex<Vec<Weak<Mutex<CustRegInner>>>>>,
    current_up_down_values: Arc<RwLock<Bitmap<{ hw_specific::NUM_KEYS }>>>,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub key_index: usize,
    pub key_char: char,
    pub key_position: KeyPosition,
    pub key_instant: Instant,
    pub key_systime: SystemTime,
    pub up_down_values: Bitmap<{ hw_specific::NUM_KEYS }>,
}

#[derive(Debug, Clone)]
struct CustRegInner {
    keydown_mask: Bitmap<{ hw_specific::NUM_KEYS }>,
    keyup_mask: Bitmap<{ hw_specific::NUM_KEYS }>,
    key_mapping: [char; hw_specific::NUM_KEYS],
    event_queue: Arc<Mutex<VecDeque<KeyEvent>>>,

    // They insist on a Mutex, even though I don't need one.
    wake_cond: Arc<(Mutex<()>, Condvar)>,
}

#[derive(Debug, Clone)]
/// Struct for getting debounced key events. To obtain one of these
/// call `keybow::register_events()`.
pub struct CustReg {
    // We need our client code to have a mutex to the registration
    // data but we will keep a weak reference to it so we can know
    // that it has gone out of scope and then delete our copy.
    //
    // But we want all of this internal stuff to be opaque to the
    // client code; the client code should just make simple method
    // calls. So it is a simple struct on the outside. On the inside?
    // The mutex we actually want the client to have.
    reg_inner: Arc<Mutex<CustRegInner>>,
    iter_timeout: Duration,
}

#[derive(Debug, Copy, Clone)]
pub struct KeyStateDebounceData {
    stable_key_position: KeyPosition,
    stable_key_instant: Instant,
    stable_key_systime: SystemTime,
    debounce_state: DebounceState,
    raw_key_position: KeyPosition,
    raw_key_instant: Instant,
    test_stable_time: Instant,
    bounce_count: u32,
    key_index: usize,
    unpark_instant: Instant,
}

#[derive(Debug, Copy, Clone)]
pub struct Point {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum DebounceState {
    Stable,   // All is quiet, thread is parked.
    Unstable, // Thread is sleeping, then checking, then sleeping…
}

#[derive(Debug)]
pub enum KeyLocation {
    Coordinate(Point),
    Index(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyPosition {
    Up,
    Down,
}

/// Look up (x,y) and get back key/LED index.
fn coord_to_index(coordinate: Point) -> Result<usize, KeybowError> {
    let two_d_array = HardwareInfo::get_info().xy_to_index_lookup;

    if coordinate.x >= two_d_array.len() || coordinate.y >= two_d_array[0].len() {
        Err(KeybowError::BadKeyLocation {
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
}
