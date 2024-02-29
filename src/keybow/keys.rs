use std::sync::OnceLock;

use crate::keybow::*;

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

    /// Creates new struct for talking to Keybow hardware; not called
    /// directly by user code.
    fn new_inner() -> Keybow {
        let gpio = Gpio::new()
            .unwrap_or_else(|_| panic!("This only runs on: {}", hw_specific::HARDWARE_NAME));

        let mut gpio_keys_vec = Vec::<Arc<Mutex<InputPin>>>::with_capacity(hw_specific::NUM_KEYS);
        for one_gpio_num in HardwareInfo::get_info().key_index_to_gpio {
            gpio_keys_vec.push(Arc::new(Mutex::new(
                gpio.get(one_gpio_num).unwrap().into_input_pullup(),
            )));
        }

        let mut keybow = Keybow {
            led_data: Arc::new(Mutex::new(
                [rgb::RGB { r: 0, g: 0, b: 0 }; hw_specific::NUM_KEYS],
            )),
            brightness: 1.0,
            spi: Arc::new(Mutex::new(
                spi::Spi::new(
                    spi::Bus::Spi0,
                    spi::SlaveSelect::Ss0,
                    1_000_000,
                    spi::Mode::Mode0,
                )
                .unwrap(),
            )),
            gpio_keys: gpio_keys_vec,
            _gpio: gpio,
            key_state_debounce_data: <Vec<Arc<Mutex<KeyStateDebounceData>>>>::with_capacity(
                hw_specific::NUM_KEYS,
            ),
            debounce_thread_vec: <Vec<Arc<Mutex<thread::JoinHandle<()>>>>>::with_capacity(
                hw_specific::NUM_KEYS,
            ),
            debounce_threshold: Duration::from_millis(10),
            cust_reg: Arc::new(Mutex::new(Vec::new())),
            current_up_down_values: Arc::new(RwLock::new(Bitmap::new())),
        };

        let now = Instant::now();
        let now_systime = SystemTime::now();

        for index in 0..hw_specific::NUM_KEYS {
            // Initialize one KeyStateDebounceData
            let key_state_position =
                KeyPosition::from(keybow.gpio_keys[index].lock().unwrap().is_low());
            let key_state_mutex = Arc::new(Mutex::new(KeyStateDebounceData {
                stable_key_position: key_state_position,
                stable_key_instant: now,
                stable_key_systime: now_systime,
                debounce_state: super::DebounceState::Stable,
                raw_key_position: key_state_position,
                raw_key_instant: now,
                test_stable_time: now,
                bounce_count: 0,
                key_index: index,
                unpark_instant: now,
            }));
            {
                let mut current_up_down_values = keybow.current_up_down_values.write().unwrap();
                current_up_down_values.set(index, bool::from(key_state_position));
            };

            // Clone and push KeyStateDebounceData into vec.
            keybow.key_state_debounce_data.push(key_state_mutex.clone());

            // What key events any customer might have registered for.
            let thread_cust_reg = keybow.cust_reg.clone();

            let thread_name = format!("keybow {}", index);
            let key_state_cloned = key_state_mutex.clone();
            let current_up_down_values_cloned = keybow.current_up_down_values.clone();
            keybow.debounce_thread_vec.push(Arc::new(Mutex::new(
                thread::Builder::new()
                    .name(thread_name)
                    .spawn(move || {
                        Keybow::debounce_thread(
                            key_state_cloned,
                            thread_cust_reg,
                            current_up_down_values_cloned,
                        )
                    })
                    .unwrap(),
            )));
        }

        for (index, one_key) in keybow.gpio_keys.iter().enumerate() {
            let one_key_state = &keybow.key_state_debounce_data[index];
            let one_debounce_thread = &keybow.debounce_thread_vec[index];

            let one_key_state_cloned = one_key_state.clone();
            let one_debounce_thread_cloned = one_debounce_thread.clone();
            let one_gpio_key_mutex_clone = keybow.gpio_keys[index].clone();

            let _ = one_key
                .lock()
                .unwrap()
                .set_async_interrupt(Trigger::Both, move |level| {
                    let one_key_state_cloned_again = one_key_state_cloned.clone();
                    let one_debounce_thread_cloned_again = one_debounce_thread_cloned.clone();
                    let one_gpio_key_mutex_clone_again = one_gpio_key_mutex_clone.clone();
                    Keybow::debounce_callback(
                        one_key_state_cloned_again,
                        KeyPosition::from(level),
                        keybow.debounce_threshold,
                        index,
                        one_debounce_thread_cloned_again,
                        one_gpio_key_mutex_clone_again,
                    );
                });
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
     * event, and park(), waiting until a new keyevent happens, and
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
     *  - `‾` is a call to `debounce_callbck()` with the key reporting
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
     *  - `T` is thread launching (have to scroll back to see this).
     *
     *
     * If the thread takes longer than 1ms to unpark we will print a
     * message saying how long it took. For example:
     *
     *   `time to unpark 2.581923ms`.
     */
    fn debounce_thread(
        key_state: Arc<Mutex<KeyStateDebounceData>>,
        thread_cust_reg: Arc<Mutex<Vec<Weak<Mutex<CustRegInner>>>>>,
        current_up_down_values: Arc<RwLock<Bitmap<{ hw_specific::NUM_KEYS }>>>,
    ) {
        crate::debug_waveform!("T");
        loop {
            let old_test_stable_time = { key_state.lock().unwrap().test_stable_time };
            crate::debug_waveform!("[");
            thread::sleep_until(old_test_stable_time);
            crate::debug_waveform!("]");

            let went_stable = {
                let mut state = key_state.lock().unwrap();

                if old_test_stable_time != state.test_stable_time {
                    false // did not go stable
                } else {
                    // did go stable
                    state.stable_key_position = state.raw_key_position;
                    state.stable_key_instant = Instant::now();
                    state.stable_key_systime = SystemTime::now();
                    state.debounce_state = DebounceState::Stable;
                    state.bounce_count = 0;

                    {
                        let mut current_up_down_values = current_up_down_values.write().unwrap();
                        current_up_down_values
                            .set(state.key_index, bool::from(state.stable_key_position));
                    };

                    crate::debug_waveform!(
                        "S:   key: {} {}\n\n:",
                        state.key_index,
                        state.stable_key_value_p
                    );

                    let mut thread_cust_reg_vec = thread_cust_reg.lock().unwrap();

                    let mut index_needs_deleting = None;

                    for (index, one_cust_reg) in thread_cust_reg_vec.iter().enumerate() {
                        let one_cust_reg_upgrade = one_cust_reg.upgrade();
                        match &one_cust_reg_upgrade {
                            Some(one_cust_reg_upgraded) => {
                                let one_cust_reg_finally = one_cust_reg_upgraded.lock().unwrap();
                                Keybow::process_one_cust_reg(
                                    *state,
                                    &one_cust_reg_finally,
                                    *current_up_down_values.read().unwrap(),
                                );
                            }
                            None => {
                                index_needs_deleting = Some(index);
                            }
                        };
                    } // We have now relinquished the thread_cust_reg mutex.

                    // If we noticed a dead cust_reg, delete it. If we
                    // notices more than one, we will only delete
                    // one. We can delete another next time a key
                    // event goes stable.
                    if let Some(index) = index_needs_deleting {
                        thread_cust_reg_vec.remove(index);
                    };

                    true // went stable
                }
            }; // We have now relinquished the state mutex.

            if went_stable {
                crate::debug_waveform!("<");
                thread::park();
                let elapsed = Instant::now() - key_state.lock().unwrap().unpark_instant;
                if elapsed > Duration::from_millis(1) {
                    eprintln!("\n time to unpark {:#?}", elapsed);
                }
                crate::debug_waveform!(">");
            } else {
                crate::debug_waveform!("+");
            }
        }
    }

    /// Updates data structures for one cust registration with new key
    /// event information.
    ///
    /// More than one client thread can register for specific key
    /// events. When a new key event happens this gets called, once
    /// for each of those registrations.
    fn process_one_cust_reg(
        state: KeyStateDebounceData,
        some_cust_reg: &CustRegInner,
        current_up_down_values: Bitmap<{ hw_specific::NUM_KEYS }>,
    ) {
        let key_event = KeyEvent {
            key_index: state.key_index,
            key_char: some_cust_reg.key_mapping[state.key_index],
            key_position: state.stable_key_position,
            key_instant: state.stable_key_instant,
            key_systime: state.stable_key_systime,
            up_down_values: current_up_down_values,
        };
        if Keybow::is_key_event_wanted(&key_event, some_cust_reg) {
            let mut circ_buf = some_cust_reg.event_queue.lock().unwrap();

            // Make room if full.
            if circ_buf.len() == circ_buf.capacity() {
                let _ = circ_buf.pop_front();
            }

            // Save event.
            circ_buf.push_back(key_event);

            // Note: the var in a condvar is required because they
            // will have spurious wakeups and they insist we not be
            // confused by them. But customers are essentially polling
            // here anyway, so I don't care.
            let (_lock, cvar) = &*(some_cust_reg.wake_cond);

            // We might have more than one customer, we don't know
            // which might be currently interested in this event, so
            // we wake them all, let them all look.
            cvar.notify_all();
        }; // We have now relinquished event_queue mutex.
    }

    /// Tests whether key event matches masks for what up/down events
    /// are wanted.
    fn is_key_event_wanted(event: &KeyEvent, cust_reg: &CustRegInner) -> bool {
        let position_bool = bool::from(event.key_position);
        (position_bool && cust_reg.keydown_mask.get(event.key_index))
            || (!position_bool && cust_reg.keyup_mask.get(event.key_index))
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
     */
    fn debounce_callback(
        key_state: Arc<Mutex<KeyStateDebounceData>>,
        _key_position: KeyPosition, // Their value seems spurious.
        threshold: Duration,
        _key_index: usize,
        debounce_thread: Arc<Mutex<thread::JoinHandle<()>>>,
        gpio_key_mutex: Arc<Mutex<rppal::gpio::InputPin>>,
    ) {
        // This code runs each time rppal thinks we had a key event.
        let now = Instant::now();

        let key_position = KeyPosition::from(gpio_key_mutex.lock().unwrap().is_low());
        match key_position {
            KeyPosition::Down => {
                crate::debug_waveform!("_");
            }
            KeyPosition::Up => {
                crate::debug_waveform!("‾");
            }
        }

        let went_unstable = {
            let mut state = key_state.lock().unwrap();

            state.bounce_count += 1;

            // We extend test_stable_time everytime we get called.
            state.test_stable_time = now + threshold;

            state.raw_key_instant = now;
            state.raw_key_position = key_position;

            if state.debounce_state == DebounceState::Stable {
                // First key event.
                state.debounce_state = DebounceState::Unstable;
                true
            } else {
                false
            }
        }; // We have now relinquished the mutex.

        if went_unstable {
            // This was a new key event, unpark thread.
            key_state.lock().unwrap().unpark_instant = Instant::now();
            debounce_thread.lock().unwrap().thread().unpark();
        }
    }

    /// The debounce threshold is how long a GPIO input must be quiet
    /// before we can conclude that the keybounce is over.
    pub fn set_debounce_threshold(&mut self, threshold: Duration) {
        self.debounce_threshold = threshold;
    }

    /// Gets Vec of all raw (not debounced) current key values.
    pub fn get_keys_raw(&self) -> Vec<bool> {
        // No debounce.
        let mut ret_val = Vec::<bool>::with_capacity(hw_specific::NUM_KEYS);

        for one_gpio_key in self.gpio_keys.iter() {
            ret_val.push(one_gpio_key.lock().unwrap().is_low())
        }

        ret_val
    }

    /// Get one raw (not debounced) key value.
    pub fn get_key_raw(&self, key_num: usize) -> Result<KeyPosition, KeybowError> {
        // No debounce.
        if key_num >= hw_specific::NUM_KEYS {
            Err(KeybowError::BadKeyLocation {
                bad_location: KeyLocation::Index(key_num),
            })
        } else {
            Ok(KeyPosition::from(
                self.gpio_keys[key_num].lock().unwrap().is_low(),
            ))
        }
    }

    /// Get one current debounced key value.
    pub fn get_key(&self, key_num: usize) -> Result<KeyPosition, KeybowError> {
        if key_num >= hw_specific::NUM_KEYS {
            Err(KeybowError::BadKeyLocation {
                bad_location: KeyLocation::Index(key_num),
            })
        } else {
            // Get all values.
            let rwlock_bitmap = self.current_up_down_values.read();

            // Return just one.
            Ok(KeyPosition::from(rwlock_bitmap.unwrap().get(key_num)))
        }
    }

    /// Get bitmap (which holds `bools`, not `KeyPosition`s,
    /// unfortunately) of all current debounced key values.
    pub fn get_keys(&self) -> Bitmap<{ hw_specific::NUM_KEYS }> {
        // Return all debounced values.
        let rwlock_bitmap = self.current_up_down_values.read();

        *rwlock_bitmap.unwrap()
    }

    /// Used by client code to register for specific key events.
    ///
    /// # Arguments
    ///
    /// * `keydown_mask` Set bit position `true` to get events for the
    ///    key with that index.
    /// * `keyup_mask` Set bit position `true` to get events for the
    ///    key with that index.
    /// * `queue_length` Number of key events to queue. If queue
    ///   overflows oldest events will be discarded.
    /// * `key_mapping` Array of `char`, each the character that will
    ///    be returned as the character for the corresponding key.
    /// * `iter_timeout` Timeout used when reading key events via
    ///    iterator. Use `None` for infinite timeout.
    pub fn register_events(
        &mut self,
        keydown_mask: Bitmap<{ hw_specific::NUM_KEYS }>,
        keyup_mask: Bitmap<{ hw_specific::NUM_KEYS }>,
        queue_length: usize,
        key_mapping: [char; hw_specific::NUM_KEYS],
        iter_timeout: Option<Duration>,
    ) -> CustReg {
        let register = CustRegInner {
            keydown_mask,
            keyup_mask,
            key_mapping,
            event_queue: Arc::new(Mutex::new(VecDeque::with_capacity(queue_length))),
            wake_cond: Arc::new((Mutex::new(()), Condvar::new())),
        };
        let register_mutex = Arc::new(Mutex::new(register));
        let register_weak = Arc::downgrade(&register_mutex);
        let mut cust_reg_vec = self.cust_reg.lock().unwrap();
        cust_reg_vec.push(register_weak);
        CustReg {
            reg_inner: register_mutex,
            iter_timeout: match iter_timeout {
                Some(timeout) => timeout,
                None => Duration::MAX,
            },
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
        match if_down {
            true => KeyPosition::Down,
            false => KeyPosition::Up,
        }
    }
}

impl CustReg {
    /// Get next key event that matches supplied mask, or None. Any
    /// older key events in the queue that do not match the mask will
    /// remain in the queue.
    pub fn get_next_key_event_masked(
        &mut self,
        mask: Bitmap<{ hw_specific::NUM_KEYS }>,
    ) -> Option<KeyEvent> {
        let cust_reg_inner = self.reg_inner.lock().unwrap();
        let mut locked_queue = cust_reg_inner.event_queue.lock().unwrap();

        let mut found_index = None;
        for (index, event) in locked_queue.iter_mut().rev().enumerate() {
            if mask.get(event.key_index) {
                found_index = Some(index);
                break;
            }
        }

        match found_index {
            Some(good_index) => locked_queue.remove(good_index),
            None => None,
        }
    }

    /// A wrapper on get_next_key_event_masked(), with an added
    /// timeout. Still might return None.
    pub fn wait_next_key_event_masked(
        &mut self,
        mask: Bitmap<{ hw_specific::NUM_KEYS }>,
        mut timeout: Duration,
    ) -> Option<KeyEvent> {
        // Limit timeout to a billion years, to avoid overflow
        // problems doing arithmetic near Duration::MAX.
        const BIG_DURATION: Duration = Duration::from_secs(1_000_000_000 * 365 * 24 * 60 * 60);
        if timeout > BIG_DURATION {
            timeout = BIG_DURATION;
        }

        // Note our original deadline only once.
        let deadline_instant = Instant::now() + timeout;

        // Call until we get a result or have passed the deadline.
        loop {
            let possible_event = self.get_next_key_event_masked(mask);

            match possible_event {
                Some(real_event) => {
                    return Some(real_event); // Return result!
                }
                None => {
                    let now = Instant::now();
                    if deadline_instant < now {
                        return None; // Past deadline, return None.
                    } else {
                        // Wait until a new event appears.
                        let new_duration = deadline_instant - Instant::now();
                        let (lock, cvar) = &*(self.reg_inner.lock().unwrap().wake_cond.clone());

                        // wait_timeout() insists on a MutexGuard, so
                        // I'll give it my _dummy.
                        let _dummy = lock.lock().unwrap();
                        let _ = cvar.wait_timeout(_dummy, new_duration).unwrap();
                        continue; // Loop!
                    }
                }
            }
        }
    }

    /// Throw away all queued key events.
    pub fn clear_queue(&mut self) {
        let cust_reg_inner = self.reg_inner.lock().unwrap();
        let mut event_queue_locked = cust_reg_inner.event_queue.lock().unwrap();

        event_queue_locked.clear();
    }

    /// Get next key event, whatever it be.
    pub fn get_next_key_event(&mut self) -> Option<KeyEvent> {
        self.reg_inner
            .lock()
            .unwrap()
            .event_queue
            .lock()
            .unwrap()
            .pop_front()
    }

    /// Get character of next event, whatever it be.
    pub fn get_next_char(&mut self) -> Option<char> {
        self.get_next_key_event().map(|event| event.key_char)
    }

    /// Wait for next key event, whatever it be.
    pub fn wait_next_key_event(&mut self, timeout: Duration) -> Option<KeyEvent> {
        self.wait_next_key_event_masked(Bitmap::mask(hw_specific::NUM_KEYS), timeout)
    }

    /// Wait for next event, return character, whatever it be.
    pub fn wait_next_char(&mut self, timeout: Duration) -> Option<char> {
        self.wait_next_key_event(timeout)
            .map(|event| event.key_char)
    }
}

impl Iterator for CustReg {
    type Item = crate::keybow::KeyEvent;

    /// Note: This iterator changes size as new key events happen.
    fn next(&mut self) -> Option<crate::keybow::KeyEvent> {
        self.wait_next_key_event(self.iter_timeout)
    }

    /// Returns (number of events currently present, Some(ring buffer capacity))
    fn size_hint(&self) -> (usize, Option<usize>) {
        let cust_reg_inner = self.reg_inner.lock().unwrap();
        let event_queue_locked = cust_reg_inner.event_queue.lock().unwrap();

        (
            event_queue_locked.len(),
            Some(event_queue_locked.capacity()),
        )
    }
}
