use crate::keybow::HardwareInfo;

// If the hardware were to have a slight change in the future, it
// might be sufficient to make changes in the section below.

pub const HARDWARE_NAME: &str = "Keybow 12-key keypad, Raspberry Pi Zero";
pub const NUM_KEYS: usize = 12;
pub const NUM_LEDS: usize = NUM_KEYS;
pub const DEBOUNCE_THRESHOLD_MS: u64 = 10;
const KEY_INDEX_TO_GPIO: [u8; NUM_KEYS] = [17, 22, 6, 20, 27, 24, 12, 16, 23, 5, 13, 26];

/// 2D array to map from (X,Y) to key/LED index.
const XY_TO_INDEX_LOOKUP: [[usize; 3]; 4] = [[0, 4, 8], [1, 5, 9], [2, 6, 10], [3, 7, 11]];

/// Map from LED position in SPI output (array index) to LED index (value).
const SPI_SEQ_TO_INDEX_LEDS: [usize; NUM_LEDS] = [3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8];

const LAYOUT_TEXT: &str = "
  Keypad numbering:

     00  01  02  03
     04  05  06  07
     08  09  10  11
           |
           |
       power wire
";

// If HW changes are minor, no changes will need to be made other than
// those make above. With luck.

static_assertions::const_assert!(
    XY_TO_INDEX_LOOKUP.len() * XY_TO_INDEX_LOOKUP[0].len() == NUM_KEYS
);
static_assertions::const_assert!(KEY_INDEX_TO_GPIO.len() == NUM_KEYS);
static_assertions::const_assert!(SPI_SEQ_TO_INDEX_LEDS.len() == NUM_LEDS);

impl HardwareInfo {
    #[must_use]
    pub const fn get_info() -> super::HardwareInfo {
        HardwareInfo {
            name: HARDWARE_NAME,
            num_keys: NUM_KEYS,
            num_leds: NUM_LEDS,
            key_index_to_gpio: KEY_INDEX_TO_GPIO,
            xy_to_index_lookup: XY_TO_INDEX_LOOKUP,
            spi_seq_to_index_leds: SPI_SEQ_TO_INDEX_LEDS,
            layout_text: LAYOUT_TEXT.to_string(),
        }
    }
}
