use crate::keybow::*;

impl Keybow {
    /// Set one LED color in off-screen frame buffer.
    pub fn set_led(
        &mut self,
        key_location: KeyLocation,
        color: rgb::RGB<u8>,
    ) -> Result<(), KeybowError> {
        let key_index = match key_location {
            KeyLocation::Index(index) => index,
            KeyLocation::Coordinate(coordinate) => coord_to_index(coordinate)?,
        };
        if key_index >= hw_specific::NUM_LEDS {
            Err(KeybowError::BadKeyLocation {
                bad_location: KeyLocation::Index(key_index),
            })
        } else {
            self.led_data.lock().unwrap()[key_index] = color;
            Ok(())
        }
    }

    /// Get one LED color from off-screen frame buffer.
    pub fn get_led(&self, key_location: KeyLocation) -> Result<rgb::RGB<u8>, KeybowError> {
        let key_index = match key_location {
            KeyLocation::Index(index) => index,
            KeyLocation::Coordinate(coordinate) => coord_to_index(coordinate)?,
        };
        if key_index >= hw_specific::NUM_LEDS {
            Err(KeybowError::BadKeyLocation {
                bad_location: KeyLocation::Index(key_index),
            })
        } else {
            Ok(self.led_data.lock().unwrap()[key_index])
        }
    }

    /// Return the entire off-screen frame buffer as a linear buffer
    /// indexed by LED index.
    pub fn get_leds(&self) -> [rgb::RGB<u8>; hw_specific::NUM_LEDS] {
        *self.led_data.lock().unwrap()
    }

    /// Set off-screen frame buffer to all off.
    pub fn clear_leds(&mut self) {
        *self.led_data.lock().unwrap() = [rgb::RGB { r: 0, g: 0, b: 0 }; hw_specific::NUM_LEDS];
    }

    /** Copies the off-screen frame buffer to the physical
     *  LEDs. Off-screen frame buffer is not altered.
     *
     *  Changes to LED colors will not be visible until you call this.
     */
    pub fn show_leds(&mut self) -> Result<(), Box<dyn Error>> {
        // This is specific to the Keybow 12-key hardware, but I am
        // generalizing it a little in case siilar and related
        // hardware comes along.

        let bright_spi = (self.brightness * 31.0) as u8 | 0b11100000;

        /* We wrote the LEDs by sending:
         *
         *  - Header of 8-bytes of zeros,
         *  - 4-bytes for each of the 12-LEDs,
         *  - Tail of 4-bytes of 255.
         */
        let mut header = vec![0_u8; 8];
        let mut tail = vec![255_u8; 4];
        let mut led_spi =
            Vec::<u8>::with_capacity(header.len() + hw_specific::NUM_LEDS * 4 + tail.len());

        led_spi.append(&mut header);
        for one_led_index in HardwareInfo::get_info().spi_seq_to_index_leds {
            let led_data_locked = self.led_data.lock().unwrap();
            led_spi.push(bright_spi);
            led_spi.push(led_data_locked[one_led_index].b);
            led_spi.push(led_data_locked[one_led_index].g);
            led_spi.push(led_data_locked[one_led_index].r);
        }
        led_spi.append(&mut tail);

        let result = self.spi.lock().unwrap().write(&led_spi);

        match result {
            Err(e) => Err(Box::new(e)),
            _ => Ok(()),
        }
    }
}
