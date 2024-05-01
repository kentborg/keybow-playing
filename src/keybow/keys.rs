use crossbeam_channel;
use std::sync::OnceLock;
use tokio;

use crate::keybow::{
    hw_specific, mutex_poison, read_poison, spi, thread, write_poison, Arc, CustReg, CustRegInner,
    DebounceState, Duration, Gpio, HardwareInfo, InputPin, Instant, KError, KeyLocation,
    KeyPosition, KeyStateDebounceData, Keybow, Level, Mutex, RwLock, SystemTime, Trigger,
    UnstableState, VecDeque, Weak,
};

use super::CustRegAsync;

static KEYBOW_GLOBAL_MUTEX: OnceLock<crate::keybow::Keybow> = OnceLock::new();

impl Default for Keybow {
    fn default() -> Self {
        Self::new()
    }
}

impl Keybow {
    /// Create new struct for talking to Keybow hardware. Memoized
    /// version of `new_inner()`, the one that does all the work.
    pub fn new() -> Keybow {
        KEYBOW_GLOBAL_MUTEX.get_or_init(Keybow::new_inner).clone()
    }

    /// Returns `Vec` of for talking to gpio input pins.
    fn get_initialized_gpio_vec(gpio: &Gpio) -> Vec<Arc<Mutex<InputPin>>> {
        let mut gpio_keys_vec = Vec::<Arc<Mutex<InputPin>>>::with_capacity(hw_specific::NUM_KEYS);
        for one_gpio_num in HardwareInfo::get_info().key_index_to_gpio {
            gpio_keys_vec.push(Arc::new(Mutex::new(
                gpio.get(one_gpio_num)
                    .unwrap_or_else(|e| {
                        panic!(
                            "{e:?}: get of gpio pin {one_gpio_num} failed, is it already in use?"
                        )
                    })
                    .into_input_pullup(),
            )));
        }

        gpio_keys_vec
    }

    /// Returns a `Keybow`, mostly initiliazed.
    fn get_mostly_inited_keybow(gpio_keys_vec: Vec<Arc<Mutex<InputPin>>>, gpio: Gpio) -> Keybow {
        Keybow {
            led_data: Arc::new(Mutex::new(
                [rgb::RGB { r: 0, g: 0, b: 0 }; hw_specific::NUM_KEYS],
            )),
            brightness: Arc::new(Mutex::new(1.0)),
            spi: Arc::new(Mutex::new(
                spi::Spi::new(
                    spi::Bus::Spi0,
                    spi::SlaveSelect::Ss0,
                    1_000_000,
                    spi::Mode::Mode0,
                )
                .unwrap_or_else(|e| {
                    panic!("{e:?}: Spi::new() failed, problem with your hardware definition?",)
                }),
            )),
            gpio_keys: gpio_keys_vec,
            _gpio: gpio,
            key_state_debounce_data: <Vec<Arc<Mutex<KeyStateDebounceData>>>>::with_capacity(
                hw_specific::NUM_KEYS,
            ),
            debounce_thread_vec: <Vec<Arc<thread::JoinHandle<()>>>>::with_capacity(
                hw_specific::NUM_KEYS,
            ),
            cust_reg: Arc::new(Mutex::new(Vec::new())),
            current_up_down_values: Arc::new(RwLock::new([KeyPosition::Up; hw_specific::NUM_KEYS])),
        }
    }

    /// Creates new struct for talking to Keybow hardware; not called
    /// directly by user code.
    fn new_inner() -> Keybow {
        let gpio = Gpio::new().unwrap_or_else(|e| {
            panic!("{:?}: This only runs on: {}", e, hw_specific::HARDWARE_NAME)
        });

        let gpio_keys_vec = Self::get_initialized_gpio_vec(&gpio);

        let mut keybow = Self::get_mostly_inited_keybow(gpio_keys_vec, gpio);

        for index in 0..hw_specific::NUM_KEYS {
            // Initialize one KeyStateDebounceData
            let key_state_position = {
                KeyPosition::from(
                    keybow.gpio_keys[index]
                        .lock()
                        .unwrap_or_else(mutex_poison)
                        .is_low(),
                )
            }; // gpio_keys[index] lock released.

            let debounce_state = DebounceState::Stable;
            let key_state_mutex = Arc::new(Mutex::new(KeyStateDebounceData {
                key_index: index,
                debounce_state,
            }));

            {
                let mut current_up_down_values = keybow
                    .current_up_down_values
                    .write()
                    .unwrap_or_else(write_poison);
                current_up_down_values[index] = key_state_position;
            };

            keybow.key_state_debounce_data.push(key_state_mutex.clone());

            // What key events any customer might have registered for.
            let thread_cust_reg = keybow.cust_reg.clone();

            let thread_name = format!("keybow {index}");
            let key_state_cloned = key_state_mutex.clone();
            let current_up_down_values_cloned = keybow.current_up_down_values.clone();
            keybow.debounce_thread_vec.push(Arc::new(
                thread::Builder::new()
                    .name(thread_name.clone())
                    .spawn(move || {
                        Keybow::debounce_thread(
                            &key_state_cloned,
                            &thread_cust_reg,
                            &current_up_down_values_cloned,
                        );
                    })
                    .unwrap_or_else(|e| panic!("{e:?}: spawn of thread {thread_name} failed")),
            ));
        }

        for (index, one_key) in keybow.gpio_keys.iter().enumerate() {
            let one_key_state = &keybow.key_state_debounce_data[index];
            let one_debounce_thread = &keybow.debounce_thread_vec[index];

            let one_key_state_cloned = one_key_state.clone();
            let one_debounce_thread_cloned = one_debounce_thread.clone();
            let one_gpio_key_mutex_clone = keybow.gpio_keys[index].clone();

            one_key
                .lock()
                .unwrap_or_else(mutex_poison)
                .set_async_interrupt(Trigger::Both, move |level| {
                    let one_key_state_cloned_again = one_key_state_cloned.clone();
                    let one_debounce_thread_cloned_again = one_debounce_thread_cloned.clone();
                    let one_gpio_key_mutex_clone_again = one_gpio_key_mutex_clone.clone();
                    Keybow::debounce_callback(
                        &one_key_state_cloned_again,
                        KeyPosition::from(level),
                        Duration::from_millis(hw_specific::DEBOUNCE_THRESHOLD_MS),
                        index,
                        &one_debounce_thread_cloned_again,
                        &one_gpio_key_mutex_clone_again,
                    );
                })
                .unwrap_or_else(|e| {
                    panic!("{e:?}: failed set_async_interrupt() for gpio pin {one_key:?}.")
                }); // one_key lock freed.
        }
        keybow
    }

    /**
     * There is one of these threads per key in the keypad. Each of
     * these threads launches once, this private function it is a big
     * loop, and it spends most of its time `park()`ed, waiting for
     * `debounce_callback()` to `unpark()` it.
     *
     * When a keyevent happens `debounce_callback()` will be called,
     * it will set the `test_stable_time` (how long we are to sleep) a
     * bit into the future, and `unpark()` us.
     *
     * We note the sleep time, then sleep for as long as we were told
     * to. When we wake we check whether the sleep time changed while
     * we were sleeping.
     *
     * If the sleep time changed, that means `debounce_callback()` is
     * still being called and still updating the sleep time, which
     * means the line is still bouncing and not stable, so we hit the
     * top of the loop, and immediately sleep again.
     *
     * If the sleep time did NOT change, then the key is stable,
     * `debounce_callback()` is no longer being called, so we note the
     * event, and `park()`, waiting until a new keyevent happens, and
     * `debounce_callback()` unparks us and we hit the top of the loop
     * again.
     *
     * When we do conclude we are stable we also set the
     * `debounce_state` to `DebounceState::Stable`, this is important
     * for communicating with the `debounce_callback()`.
     *
     * The `debug_waveform!()` macros are useful for seeing the
     * debouncing in action. To turn them on
     *
     *  `cargo run --features debug_waveform_print`.
     *
     * Examples:
     *
     *  `:<‾>[]S:   key: 0` Key 0 being pressed.
     *
     *  `:<_>[___‾_‾]+[]S:   key: 2`  Key 2 being released.
     *
     *  Key:
     *
     *  - `:` Printed at the beginning and end of the "waveform".
     *
     *  - `<` is before the thread is parked.
     *
     *  - `>` is after the thread us unparked. (Anything between the
     *    two happened before the thread unparked.)
     *
     *  - `_` is a call to `debounce_callback()` with key reporting
     *    down.
     *
     *  - `‾` is a call to `debounce_callback()` with the key reporting
     *    up.
     *
     *  - `[` is before the thread sleeps.
     *
     *  - `]` is after thread wakes. (Anything between the two
     *    happened while the thread was sleeping.)
     *
     *  - `+` is awakened thread concluded we are not stable, will
     *    sleep again.
     *
     *  - `S` is thread concluding we are stable.
     *
     */
    fn debounce_thread(
        key_state: &Arc<Mutex<KeyStateDebounceData>>,
        thread_cust_reg: &Arc<Mutex<Vec<Weak<Mutex<CustRegInner>>>>>,
        current_up_down_values: &Arc<RwLock<[KeyPosition; hw_specific::NUM_KEYS]>>,
    ) {
        fn get_index_unstable_state(
            key_state: &Arc<Mutex<KeyStateDebounceData>>,
        ) -> (usize, UnstableState) {
            let key_state_debounce_data = key_state.lock().unwrap_or_else(mutex_poison);
            match key_state_debounce_data.debounce_state {
                DebounceState::Stable => {
                    panic!("debounce_thread() unparked in DebounceState::Stable state");
                }
                DebounceState::Unstable(key_state) => {
                    (key_state_debounce_data.key_index, key_state)
                }
            }
        } // key_state lock freed.

        thread::park(); // We start parked.
        loop {
            // Sleep as we are told.
            crate::debug_waveform!("[");
            let (key_index, unstable_state) = get_index_unstable_state(key_state);
            thread::sleep_until(unstable_state.test_stable_time);
            let (key_index_after, unstable_state_after) = get_index_unstable_state(key_state);
            crate::debug_waveform!("]");

            assert!(key_index == key_index_after);

            if unstable_state.test_stable_time != unstable_state_after.test_stable_time {
                // Time changed, still activity, wait for things to stop, sleep…
                crate::debug_waveform!("+");
                continue;
            }

            // Fell through, sleep time did not change, key is stable!
            crate::debug_waveform!(
                "S: key: {} {:?}\n\n:",
                key_index,
                unstable_state.raw_key_position
            );

            // Set stable state.
            {
                let mut key_state_debounce_data = key_state.lock().unwrap_or_else(mutex_poison);
                key_state_debounce_data.debounce_state = DebounceState::Stable;
            } // key_state lock freed.

            // Update complete key map.
            {
                let mut up_dn_values = current_up_down_values.write().unwrap_or_else(write_poison);
                up_dn_values[key_index] = unstable_state.raw_key_position;
            } // current_up_down_values lock freed.

            // Turn this into events anyone has registered for.
            {
                let mut thread_cust_reg_vec = thread_cust_reg.lock().unwrap_or_else(mutex_poison);
                let mut index_needs_deleting = None;
                for (index, one_cust_reg) in thread_cust_reg_vec.iter().enumerate() {
                    let one_cust_reg_upgrade = one_cust_reg.upgrade();
                    match &one_cust_reg_upgrade {
                        Some(one_cust_reg_upgraded) => {
                            let mut one_cust_reg_finally =
                                one_cust_reg_upgraded.lock().unwrap_or_else(mutex_poison);
                            one_cust_reg_finally.process_one_event(
                                key_index,
                                unstable_state.raw_key_position,
                                Instant::now(),
                                SystemTime::now(),
                                *current_up_down_values.read().unwrap_or_else(read_poison),
                            );
                        }
                        None => {
                            // No one cares about this reg, client
                            // code deleted their copy, we'll delete
                            // ours, too.
                            index_needs_deleting = Some(index);
                        }
                    };
                } // one_cust_reg lock freed.

                if let Some(index) = index_needs_deleting {
                    // Theoretically could be more than one that needs deleting, but
                    // unlikely, so delete one now. Delete any other next keystroke.
                    thread_cust_reg_vec.remove(index);
                };
            } // thread_cust_reg lock freed.

            // Park ourselves until more key activity and debounce_callback() unparks us.
            crate::debug_waveform!("<");
            thread::park();
            crate::debug_waveform!(">");
        }
    }

    /**
     * This private function gets registered with rppal GPIO code,
     * there is one of these per keypad key. It gets called when rppal
     * thinks there is activity on the GPIO line. The *value* that
     * gets passed in `key_state` is not what you might expect; the
     * function might get called 5-times in a row, all with the key
     * level "high". Um, okay. I guess we will deal with that.
     *
     * This function and `debounce_thread()` do a dance. We get called
     * everytime rppal thinks there has been activity, and we note the
     * time. The `debounce_thread()` keeps sleeping, waiting until
     * nothing happended while it was sleeping, then we are all
     * debounced.
     *
     * For a little more detail, we do essentially 3 things.
     *
     * 1. Call the `debug_waveform!()` macro to show whether we are
     *    being told high or low.
     *
     * 1. Update a few items, most notably, the `test_stable_time`,
     *    this is an `Instant` a bit into the future, a time at which
     *    we might conclude bouncing has stopped and things are
     *    stable. Providing things *have* stopped and we are no longer
     *    being called.
     *
     * 1. Note whether we were called while the key was in a
     *    `DebounceState::Stable`. If so, change the state to
     *    `DebounceState::Unstable`, and `unpark()` the thread.
     *
     * This code runs each time rppal thinks we had a key event.
     */
    fn debounce_callback(
        key_state: &Arc<Mutex<KeyStateDebounceData>>,
        _key_position: KeyPosition, // The value they pass seems spurious.
        threshold: Duration,
        _key_index: usize,
        debounce_thread: &Arc<thread::JoinHandle<()>>,
        gpio_key_mutex: &Arc<Mutex<rppal::gpio::InputPin>>,
    ) {
        // Their key_position appears bad, we'll look for ourselves.
        let key_position =
            { KeyPosition::from(gpio_key_mutex.lock().unwrap_or_else(mutex_poison).is_low()) }; // gpio lock freed.

        match key_position {
            KeyPosition::Down => {
                crate::debug_waveform!("_");
            }
            KeyPosition::Up => {
                crate::debug_waveform!("‾");
            }
        };

        let went_unstable = {
            let mut key_state_debounce_data = key_state.lock().unwrap_or_else(mutex_poison);

            let went_unstable = {
                match key_state_debounce_data.debounce_state {
                    DebounceState::Unstable(_) => false,
                    DebounceState::Stable => true,
                }
            }; // key_state lock freed.

            key_state_debounce_data.debounce_state = DebounceState::Unstable(UnstableState {
                raw_key_position: key_position,
                test_stable_time: Instant::now() + threshold,
            });

            went_unstable
        }; // We have now relinquished the mutex debounce thread will need.

        if went_unstable {
            // This was a new key event, unpark debounce thread.
            debounce_thread.thread().unpark();
        }
    }

    /// Get one current debounced key value. This is not a key event,
    /// but current position; repeated calls on a key that isn't
    /// changing will return the same value repeatedly.
    ///
    /// # Errors
    ///
    /// Will return `KError::BadKeyLocation` error if passed a
    /// non-existent `key_num`.
    pub fn get_key(&self, key_num: usize) -> Result<KeyPosition, KError> {
        if key_num >= hw_specific::NUM_KEYS {
            Err(KError::BadKeyLocation {
                bad_location: KeyLocation::Index(key_num),
            })
        } else {
            // Get all values.
            let rwlock_up_downs = self
                .current_up_down_values
                .read()
                .unwrap_or_else(read_poison);

            // Return just one.
            Ok(rwlock_up_downs[key_num])
        }
    }

    /// Returns array of `KeyPosition`, one for each key.
    pub fn get_keys(&self) -> [KeyPosition; hw_specific::NUM_KEYS] {
        // Return all debounced values.
        let rwlock_up_downs = self
            .current_up_down_values
            .read()
            .unwrap_or_else(read_poison);

        *rwlock_up_downs
    }

    /// Used by client code to register for specific key events.
    ///
    /// # Arguments
    ///
    /// * `keydown_mask` Array of bool, one for each key, true if you
    ///    want down events.
    /// * `keyup_mask` Array of bool, one for each key, true if you
    ///    want up events.
    /// * `queue_length` Number of key events to queue. If queue
    ///    overflows oldest events will be discarded.
    /// * `key_mapping` Array of `char`, each the character that will
    ///    be returned as the character for the corresponding key.
    /// * `iter_timeout` Timeout used when reading key events via
    ///    iterator.
    pub fn register_events_blocking(
        &mut self,
        keydown_mask: [bool; hw_specific::NUM_KEYS],
        keyup_mask: [bool; hw_specific::NUM_KEYS],
        queue_length: usize,
        key_mapping: [char; hw_specific::NUM_KEYS],
        iter_timeout: Duration,
    ) -> CustReg {
        // Parameter is size of crossbeam_channel.
        let (cb_sender, cb_receiver) = crossbeam_channel::bounded(1);

        // Parameter is intial value in tokio::sync::watch.
        let (tk_sender, tk_receiver) = tokio::sync::watch::channel(());

        let register = CustRegInner {
            keydown_mask,
            keyup_mask,
            key_mapping,
            event_queue: Arc::new(Mutex::new(VecDeque::with_capacity(queue_length))),
            async_sender: tk_sender,
            async_receiver: tk_receiver,
            sync_sender: cb_sender,
            sync_receiver: cb_receiver,
            event_count: 0,
        };
        let register_mutex = Arc::new(Mutex::new(register));
        let register_weak = Arc::downgrade(&register_mutex);
        let mut cust_reg_vec = self.cust_reg.lock().unwrap_or_else(mutex_poison);
        cust_reg_vec.push(register_weak);
        CustReg {
            reg_inner: register_mutex,
            iter_timeout,
            queue_capacity: queue_length,
        }
    } // cust_reg lock freed.

    pub fn register_events_async(
        &mut self,
        keydown_mask: [bool; hw_specific::NUM_KEYS],
        keyup_mask: [bool; hw_specific::NUM_KEYS],
        queue_length: usize,
        key_mapping: [char; hw_specific::NUM_KEYS],
        iter_timeout: Duration,
    ) -> CustRegAsync {
        let cust_reg = self.register_events_blocking(
            keydown_mask,
            keyup_mask,
            queue_length,
            key_mapping,
            iter_timeout,
        );

        CustRegAsync {
            real_cust_reg: cust_reg,
            iter_timeout, // TODO: Make changing this have effect.
            queue_capacity: queue_length,
        }
    }
}

impl From<Level> for KeyPosition {
    fn from(level: Level) -> Self {
        match level {
            Level::High => KeyPosition::Down,
            Level::Low => KeyPosition::Up,
        }
    }
}

impl From<KeyPosition> for bool {
    fn from(key_position: KeyPosition) -> bool {
        match key_position {
            KeyPosition::Up => false,
            KeyPosition::Down => true,
        }
    }
}

impl From<bool> for KeyPosition {
    fn from(if_down: bool) -> Self {
        if if_down {
            KeyPosition::Down
        } else {
            KeyPosition::Up
        }
    }
}
