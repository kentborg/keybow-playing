use crate::keybow::{hw_specific, mutex_poison, CustRegAsync, KeyEvent};

// Heavily dependent on the synchronous code to do the real work. This
// async code will hold regular mutexes, etc., but never for any
// length of time.

impl CustRegAsync {
    /// Get next key event, whatever it be.
    pub fn get_next_key_event(&mut self) -> Option<KeyEvent> {
        self.real_cust_reg.get_next_key_event()
    }

    /// Get next key event that matches supplied mask, or `None`. Any
    /// older key events in the queue that do not match the mask will
    /// remain in the queue.
    pub fn get_next_key_event_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
    ) -> Option<KeyEvent> {
        self.real_cust_reg.get_next_key_event_masked(mask)
    }

    /// Get a copy of the first `requested_number` queued events,
    /// leaving them in the queue, all unchanged.
    ///
    /// If there are not that many events queued, return a copy of as
    /// many as there are. If there are none return `None`.
    pub fn peek_next_n_events(&mut self, requested_number: usize) -> Option<Vec<KeyEvent>> {
        self.real_cust_reg.peek_next_n_events(requested_number)
    }

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
        self.real_cust_reg
            .get_next_n_events_masked(mask, requested_number)
    }

    /// Await for next key event, whatever it be.
    pub async fn wait_next_key_event(&mut self) -> Option<KeyEvent> {
        self.wait_next_key_event_masked([true; hw_specific::NUM_KEYS])
            .await
    }

    /// Await for one event.
    ///
    /// # Panics
    ///
    /// In the case of a logic error where asking
    /// `wait_next_n_key_events_masked()` for one event giving us some
    /// other number.
    pub async fn wait_next_key_event_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
    ) -> Option<KeyEvent> {
        match self.wait_next_n_key_events_masked(mask, 1).await {
            Some(events) => {
                assert!(events.len() == 1);
                let one_event = events[0];
                Some(one_event)
            }
            None => None,
        }
    }

    /// Return exactly `requested_number` events.
    ///
    /// # Panics
    ///
    /// Will panic if `tokio::sync::watch::changed()` returns an
    /// error.
    pub async fn wait_next_n_key_events_masked(
        &mut self,
        mask: [bool; hw_specific::NUM_KEYS],
        requested_number: usize,
    ) -> Option<Vec<KeyEvent>> {
        // There is a possible race condition here:
        //
        // 1. We look for events, don't find all we need.
        //
        // 2. Another event comes in (after #1).
        //
        // 3. We then wait for a *new* event but the wait misses the
        //    event that came in at #2.
        //
        // We avoid that we tell tokio::sync::watch that we are
        // up-to-date before we go look, so when we wait we will catch
        // any subsequent updates.
        let mut chan_read = {
            let reg_inner_locked = self
                .real_cust_reg
                .reg_inner
                .lock()
                .unwrap_or_else(mutex_poison);
            reg_inner_locked.async_receiver.clone()
        }; // reg_inner lock freed.
        let _ = chan_read.borrow_and_update(); // Value doesn't matter, just that we read it.

        // Call until we get a result.
        loop {
            // We might call this when no event is ready for us. That's okay.
            let possible_event = self.get_next_n_events_masked(mask, requested_number);

            if let Some(real_event) = possible_event {
                return Some(real_event); // Return result!
            }

            // Await until a new event appears.
            match chan_read.changed().await {
                Ok(()) => {
                    continue;
                }
                Err(e) => {
                    panic!("tokio::sync::watch unexpected error: {e:?}");
                }
            };
        }
    }

    /// Get character of next event, whatever it be, is any..
    pub fn get_next_char(&mut self) -> Option<char> {
        self.real_cust_reg.get_next_char()
    }

    /// Await for next event, return character, whatever it be.
    pub async fn wait_next_char(&mut self) -> Option<char> {
        self.wait_next_key_event().await.map(|event| event.char)
    }

    /// Throw away all queued key events for this `CustRegAsync`.
    pub fn clear_queue(&mut self) {
        let cust_reg_inner = self
            .real_cust_reg
            .reg_inner
            .lock()
            .unwrap_or_else(mutex_poison);
        let mut event_queue_locked = cust_reg_inner
            .event_queue
            .lock()
            .unwrap_or_else(mutex_poison);

        event_queue_locked.clear();
    } // reg_inner lock freed; event_queue lock freed.
}

impl Iterator for CustRegAsync {
    type Item = crate::keybow::KeyEvent;

    fn next(&mut self) -> Option<KeyEvent> {
        todo!(); // TODO: Can't do await version?
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.real_cust_reg.size_hint()
    }
}
