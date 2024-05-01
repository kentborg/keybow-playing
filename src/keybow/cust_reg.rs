use crate::keybow::{
    hw_specific, mutex_poison, try_send_error, CustReg, CustRegInner, KeyEvent, KeyPosition,
};
use crossbeam_channel;
use std::time::{Duration, Instant, SystemTime};

impl CustReg {
    /// Get next key event, whatever it be.
    pub fn get_next_key_event(&mut self) -> Option<KeyEvent> {
        self.reg_inner
            .lock()
            .unwrap_or_else(mutex_poison)
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison)
            .pop_front()
    } // reg_inner lock freed; event_queue lock freed.

    /// Get next key event that matches supplied mask, or `None`. Any
    /// older key events in the queue that do not match the mask will
    /// remain in the queue.
    pub fn get_next_key_event_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
    ) -> Option<KeyEvent> {
        self.get_next_n_events_masked(mask, 1).map(|x| x[0])
    }

    /// Get a copy of the first `requested_number` queued events,
    /// leaving them in the queue, all unchanged.
    ///
    /// If there are not that many events queued, return a copy of as
    /// many as there are. If there are none return `None`.
    pub fn peek_next_n_events(&mut self, mut requested_number: usize) -> Option<Vec<KeyEvent>> {
        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let locked_queue = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        if requested_number > locked_queue.len() {
            requested_number = locked_queue.len();
        }

        let mut peek_vec: Vec<KeyEvent> = Vec::with_capacity(requested_number);
        for index in 0..requested_number {
            peek_vec.push(locked_queue[index]);
        }

        if peek_vec.is_empty() {
            None
        } else {
            Some(peek_vec)
        }
    } // reg_inner lock freed; event_queue lock freed.

    /// Return `requested_number` of matching events in an `Option`,
    /// or if there aren't that many, return `None` (leaving event
    /// queue unchanged).
    ///
    /// # Panics
    ///
    /// In the case of a logic error where an entry appears to vanish
    /// from the queue between the time when we found it and the time
    /// when we try to remove it, we will panic.
    pub fn get_next_n_events_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
        requested_number: usize,
    ) -> Option<Vec<KeyEvent>> {
        let mut found_indices = Vec::with_capacity(self.queue_capacity);

        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let mut locked_queue = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        for (index, event) in locked_queue.iter_mut().enumerate() {
            if mask[event.key_index] {
                found_indices.push(index);
                if found_indices.len() == requested_number {
                    break;
                };
            };
        }

        if found_indices.len() == requested_number {
            // Our found indices are oldered oldest to newest. To remove
            // them from the queue one at a time by index we need to
            // remove in the opposite order: newest to oldest (highest
            // index to lowest index).

            let mut found_events: Vec<KeyEvent> = Vec::with_capacity(self.queue_capacity);
            for index in found_indices.iter().rev() {
                let Some(one_event) = locked_queue.remove(*index) else {
                    // Something is very broken here, we just SAW an
                    // item with that index, but now it is gone?
                    panic!("event vanished from locked_queue vec");
                };
                found_events.push(one_event);
            }

            // We collected them backwards, so reverse them before returning.
            found_events.reverse();

            Some(found_events)
        } else {
            // We return all that were asked for, or none.
            None
        }
    } // reg_inner lock freed; event_queue lock freed.

    /// Synchronous wait for next key event, whatever it be.
    pub fn wait_next_key_event(&mut self, timeout: Duration) -> Option<KeyEvent> {
        self.wait_next_key_event_masked([true; hw_specific::NUM_KEYS], timeout)
    }

    /// Synchronous wait for one event.
    ///
    /// # Panics
    ///
    /// In the case of a logic error where asking
    /// `wait_next_n_key_events_masked()` for one event giving us some
    /// other number.
    pub fn wait_next_key_event_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
        timeout: Duration,
    ) -> Option<KeyEvent> {
        match self.wait_next_n_key_events_masked(mask, 1, timeout) {
            Some(events) => {
                assert!(events.len() == 1);
                let one_event = events[0];
                Some(one_event)
            }
            None => None,
        }
    }

    /// Synchronous wait for events.
    ///
    /// # Panics
    ///
    /// A `Disconnected` error from cross-beam `recv_timeout()` will
    /// panic.
    pub fn wait_next_n_key_events_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
        requested_number: usize,
        mut timeout: Duration,
    ) -> Option<Vec<KeyEvent>> {
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
            let possible_event = self.get_next_n_events_masked(mask, requested_number);

            if let Some(real_event) = possible_event {
                return Some(real_event); // Return result!
            }

            let now = Instant::now();
            if deadline_instant < now {
                return None; // Past deadline, return None.
            }

            // Wait until a new event appears.
            //
            // A worry in code like this is the possibility of missing
            // events. Might we sit in the following call even though
            // there is an event we could read?
            //
            // No. The possible race condition would be that we read
            // the buffer at the top of this loop, and then a new
            // event arrived before we get to the following. Well,
            // then a new message will be waiting. We receive it
            // below, removing it, and loop again.
            let chan_read = {
                let reg_inner_locked = self.reg_inner.lock().unwrap_or_else(mutex_poison);
                reg_inner_locked.sync_receiver.clone()
            }; // reg_inner lock freed.

            let read_val = chan_read.recv_timeout(deadline_instant - now);
            match read_val {
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return None;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "crossbeam_channel::Receiver::recv_timeout() unexpected Disconnected error"
                    );
                }
                Ok(_) => {
                    // We don't care about the value.
                    continue;
                }
            };
        }
    }

    /// Get character of next event, whatever it be, if any.
    pub fn get_next_char(&mut self) -> Option<char> {
        self.get_next_key_event().map(|event| event.char)
    }

    /// Synchronous wait for next event, return character, whatever it be.
    pub fn wait_next_char(&mut self, timeout: Duration) -> Option<char> {
        self.wait_next_key_event(timeout).map(|event| event.char)
    }

    /// Throw away all queued key events for this `CustReg`.
    pub fn clear_queue(&mut self) {
        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let mut event_queue_locked = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        event_queue_locked.clear();
    } // reg_inner lock freed; event_queue lock freed.
}

impl Iterator for CustReg {
    type Item = crate::keybow::KeyEvent;

    /// Read key events as iterator.
    ///
    /// Note: This iterator changes size as new key events happen.
    fn next(&mut self) -> Option<KeyEvent> {
        self.wait_next_key_event(self.iter_timeout)
    }

    /// Returns number of events currently queue for this `CustReg`.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let cust_reg_inner = self.reg_inner.lock().unwrap_or_else(mutex_poison);
        let event_queue_locked = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        (event_queue_locked.len(), Some(event_queue_locked.len()))
    } // reg_inner lock freed; event_queue lock freed.
}

impl CustRegInner {
    fn is_key_event_wanted(&self, event: &KeyEvent) -> bool {
        let key_index = event.key_index;

        match event.key_position {
            KeyPosition::Down => self.keydown_mask[key_index],
            KeyPosition::Up => self.keyup_mask[key_index],
        }
    }

    pub fn process_one_event(
        &mut self,
        key_index: usize,
        key_position: KeyPosition,
        key_instant: Instant,
        key_systime: SystemTime,
        current_up_down_values: [KeyPosition; hw_specific::NUM_KEYS],
    ) {
        let mut key_event = KeyEvent {
            key_index,
            char: self.key_mapping[key_index],
            key_position,
            event_instant: key_instant,
            event_systime: key_systime,
            up_down_values: current_up_down_values,
            event_num: 0,
            additional_events_still_queued: 0,
        };

        if self.is_key_event_wanted(&key_event) {
            self.event_count += 1;
            {
                let mut circ_buf = self.event_queue.lock().unwrap_or_else(mutex_poison);

                // Make room if full.
                if circ_buf.len() == circ_buf.capacity() {
                    let _ = circ_buf.pop_front();
                }

                // Update those dummy values in key_event.
                key_event.event_num = self.event_count;
                key_event.additional_events_still_queued = circ_buf.len() + 1;

                // Save event.
                circ_buf.push_back(key_event);
            } // event_queue lock freed.

            // Wake any waiting sync subscriber.
            //
            // Make room if necessary..
            if self.sync_sender.is_full() {
                let _ = self.sync_receiver.try_recv();
            }
            // Wake if waiting sync subscriber.
            self.sync_sender
                .try_send(key_event.event_num)
                .unwrap_or_else(try_send_error);

            // Wake if waiting async suscriber.
            match self.async_sender.send(()) {
                Ok(()) => {}
                Err(e) => {
                    panic!("unexpected error from tokio::sync::watch::send(): {e:?}");
                }
            };
        }; // We have now relinquished event_queue mutex.
    }
}
