use std::sync::OnceLock;

use crate::keybow::{
    hw_specific, mutex_poison, read_poison, spi, thread, wait_timeout_poison, write_poison, Arc,
    Condvar, CustReg, CustRegInner, DebounceState, Duration, Gpio, HardwareInfo, InputPin, Instant,
    KError, KeyEvent, KeyLocation, KeyPosition, KeyStateDebounceData, Keybow, Level, Mutex, RwLock,
    SystemTime, Trigger, UnstableState, VecDeque, Weak,
};

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
            let key_state_position = KeyPosition::from(
                keybow.gpio_keys[index]
                    .lock()
                    .unwrap_or_else(mutex_poison)
                    .is_low(),
            );
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
        }

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
            }

            // Update complete key map.
            {
                let mut up_dn_values = current_up_down_values.write().unwrap_or_else(write_poison);
                up_dn_values[key_index] = unstable_state.raw_key_position;
            }

            // Turn this into events anyone has registered for.
            {
                let mut thread_cust_reg_vec = thread_cust_reg.lock().unwrap_or_else(mutex_poison);
                let mut index_needs_deleting = None;
                for (index, one_cust_reg) in thread_cust_reg_vec.iter().enumerate() {
                    let one_cust_reg_upgrade = one_cust_reg.upgrade();
                    match &one_cust_reg_upgrade {
                        Some(one_cust_reg_upgraded) => {
                            let one_cust_reg_finally =
                                one_cust_reg_upgraded.lock().unwrap_or_else(mutex_poison);
                            Keybow::process_one_cust_reg(
                                key_index,
                                unstable_state.raw_key_position,
                                Instant::now(),
                                SystemTime::now(),
                                &one_cust_reg_finally,
                                //*current_up_down_values.read_or_panic(),
                                *current_up_down_values.read().unwrap_or_else(read_poison),
                            );
                        }
                        None => {
                            // No one cares about this reg, client
                            // code deleted their copy, we'll delete
                            // our, too.
                            index_needs_deleting = Some(index);
                        }
                    };
                }

                if let Some(index) = index_needs_deleting {
                    // Theoretically could be more than one that needs deleting, but
                    // unlikely, so delete one now. Delete any other next keystroke.
                    thread_cust_reg_vec.remove(index);
                };
            }

            // Park ourselves until more key activity and debounce_callback() unparks us.
            crate::debug_waveform!("<");
            thread::park();
            crate::debug_waveform!(">");
        }
    }

    /// Updates data structures for one cust registration with new key
    /// event information.
    ///
    /// More than one client thread can register for specific key
    /// events. When a new key event happens this gets called, once
    /// for each of those registrations.
    fn process_one_cust_reg(
        key_index: usize,
        key_position: KeyPosition,
        key_instant: Instant,
        key_systime: SystemTime,
        some_cust_reg: &CustRegInner,
        current_up_down_values: [KeyPosition; hw_specific::NUM_KEYS],
    ) {
        let key_event = KeyEvent {
            key_index,
            key_char: some_cust_reg.key_mapping[key_index],
            key_position,
            key_instant,
            key_systime,
            up_down_values: current_up_down_values,
        };
        if Keybow::is_key_event_wanted(&key_event, some_cust_reg) {
            let mut circ_buf = some_cust_reg
                .event_queue
                .lock()
                .unwrap_or_else(mutex_poison);

            // Make room if full.
            if circ_buf.len() == circ_buf.capacity() {
                let _ = circ_buf.pop_front();
            }

            // Save event.
            circ_buf.push_back(key_event);

            // Let any waiting code know there is a new event. On the
            // theory that there are not many clients, we wake them
            // all. Whether they will be interested in this event or
            // not.

            // Note: the "var" Mutex in a condvar is required because
            // they can have spurious wakeups and they insist we not
            // be confused by them.
            let (_lock, cvar) = &*(some_cust_reg.wake_cond);

            // We might have more than one customer, but we assume not
            // many. We don't know which might be currently interested
            // in this event, so we wake them all, let them all look
            // at it.
            cvar.notify_all();
        }; // We have now relinquished event_queue mutex.
    }

    /// Tests whether key event matches masks for what up/down events
    /// are wanted.
    fn is_key_event_wanted(event: &KeyEvent, cust_reg: &CustRegInner) -> bool {
        let key_index = event.key_index;

        match event.key_position {
            KeyPosition::Down => cust_reg.keydown_mask[key_index],
            KeyPosition::Up => cust_reg.keyup_mask[key_index],
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
        _key_position: KeyPosition, // Their value seems spurious.
        threshold: Duration,
        _key_index: usize,
        debounce_thread: &Arc<thread::JoinHandle<()>>,
        gpio_key_mutex: &Arc<Mutex<rppal::gpio::InputPin>>,
    ) {
        // Their key_position appears bad, we'll look for ourselves.
        let key_position =
            KeyPosition::from(gpio_key_mutex.lock().unwrap_or_else(mutex_poison).is_low());

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
            };

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

    /// Get one current debounced key value.
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
    pub fn register_events(
        &mut self,
        keydown_mask: [bool; hw_specific::NUM_KEYS],
        keyup_mask: [bool; hw_specific::NUM_KEYS],
        queue_length: usize,
        key_mapping: [char; hw_specific::NUM_KEYS],
        iter_timeout: Duration,
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
        let mut cust_reg_vec = self.cust_reg.lock().unwrap_or_else(mutex_poison);
        cust_reg_vec.push(register_weak);
        CustReg {
            reg_inner: register_mutex,
            iter_timeout,
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

impl CustReg {
    /// Get next key event that matches supplied mask, or `None`. Any
    /// older key events in the queue that do not match the mask will
    /// remain in the queue.
    pub fn get_next_key_event_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
    ) -> Option<KeyEvent> {
        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let mut locked_queue = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        let mut found_index = None;
        for (index, event) in locked_queue.iter_mut().rev().enumerate() {
            if mask[event.key_index] {
                found_index = Some(index);
                break;
            }
        }

        match found_index {
            Some(good_index) => locked_queue.remove(good_index),
            None => None,
        }
    }

    /// A wrapper on `get_next_key_event_masked()`, with an added
    /// timeout. Still might return `None`.
    pub fn wait_next_key_event_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
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
            // We might call this when no event is ready for us. That's okay.
            let possible_event = self.get_next_key_event_masked(mask);

            if let Some(real_event) = possible_event {
                return Some(real_event); // Return result!
            }

            let now = Instant::now();
            if deadline_instant < now {
                return None; // Past deadline, return None.
            }

            // Else wait until a new event appears. It might not be an event for us

            let new_duration = deadline_instant - Instant::now();
            let (lock, cvar) = &*(self
                .reg_inner
                .lock()
                .unwrap_or_else(mutex_poison)
                .wake_cond
                .clone());

            // wait_timeout() insists on a MutexGuard, so
            // I'll give it my _dummy.
            let dummy = lock.lock().unwrap_or_else(mutex_poison);
            let _ = cvar
                .wait_timeout(dummy, new_duration)
                .unwrap_or_else(wait_timeout_poison);
        }
    }

    /// Throw away all queued key events.
    pub fn clear_queue(&mut self) {
        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let mut event_queue_locked = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        event_queue_locked.clear();
    }

    /// Get next key event, whatever it be.
    pub fn get_next_key_event(&mut self) -> Option<KeyEvent> {
        self.reg_inner
            .lock()
            .unwrap_or_else(mutex_poison)
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison)
            .pop_front()
    }

    /// Get character of next event, whatever it be.
    pub fn get_next_char(&mut self) -> Option<char> {
        self.get_next_key_event().map(|event| event.key_char)
    }

    /// Wait for next key event, whatever it be.
    pub fn wait_next_key_event(&mut self, timeout: Duration) -> Option<KeyEvent> {
        self.wait_next_key_event_masked([true; hw_specific::NUM_KEYS], timeout)
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
    fn next(&mut self) -> Option<KeyEvent> {
        self.wait_next_key_event(self.iter_timeout)
    }

    /// Returns (number of events currently present, Some(ring buffer capacity))
    fn size_hint(&self) -> (usize, Option<usize>) {
        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let event_queue_locked = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        (
            event_queue_locked.len(),
            Some(event_queue_locked.capacity()),
        )
    }
}
