use crate::keybow::*;

pub const HARDWARE_NAME: &str = "Keybow 12-key keypad, Raspberry Pi Zero";

pub const NUM_KEYS: usize = 12;
pub const NUM_LEDS: usize = NUM_KEYS;

const KEY_INDEX_TO_GPIO: [u8; NUM_KEYS] = [17, 22, 6, 20, 27, 24, 12, 16, 23, 5, 13, 26];

/// 2D array to map from (X,Y) to key/LED index.
const XY_TO_INDEX_LOOKUP: [[usize; 3]; 4] = [[0, 4, 8], [1, 5, 9], [2, 6, 10], [3, 7, 11]];

/// Map from LED position in SPI output (array index) to LED index (value).
const SPI_SEQ_TO_INDEX_LEDS: [usize; 12] = [3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8];

/** Hardware specific information should be contained to this file.
 *
 *  Keypad numbering:
 *
 *     00  01  02  03
 *     04  05  06  07
 *     08  09  10  11
 *           |
 *           |
 *       power wire
 *
 */
impl HardwareInfo {
    pub const fn get_info() -> super::HardwareInfo {
        HardwareInfo {
            name: HARDWARE_NAME,
            num_keys: NUM_KEYS,
            num_leds: NUM_LEDS,
            key_index_to_gpio: KEY_INDEX_TO_GPIO,
            xy_to_index_lookup: XY_TO_INDEX_LOOKUP,
            spi_seq_to_index_leds: SPI_SEQ_TO_INDEX_LEDS,
        }
    }
}
