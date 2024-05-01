use std::thread;

use rgb::RGB;
use std::io::{self, Write};
use std::time::Duration;

use crate::keybow::hw_specific;
use crate::keybow::HardwareInfo;
use keybow::keybow;

const RED: RGB<u8> = RGB { r: 255, g: 0, b: 0 };
const GREEN: RGB<u8> = RGB { r: 0, g: 255, b: 0 };
const BLUE: RGB<u8> = RGB { r: 0, g: 0, b: 255 };

#[test]
fn test_get_next_n_events_masked() {
    let stderr = io::stderr();
    let mut handle = stderr.lock();

    let hw_info = HardwareInfo::get_info();

    let complete_chars = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b'];

    let mut keybow = keybow::Keybow::new();

    let _ = handle.write_all(
        b"\nWith no keys pressed, should get no events back from get_next_n_events_masked()\n",
    );

    let mut reg = keybow.register_events_blocking(
        [true; hw_specific::NUM_KEYS],
        [false; hw_specific::NUM_KEYS],
        20,
        complete_chars,
        Duration::MAX,
    );

    if let Some(x) = reg.get_next_n_events_masked([true; 12], 5) {
        panic!("if no keys have been pressed, should have been none,  got: {x:?}");
    } else {
        let _ = handle.write_all(b"...and that worked.");
    }

    keybow.clear_leds();
    let _ = keybow.set_led(&keybow::KeyLocation::Index(0), BLUE);
    let _ = keybow.set_led(&keybow::KeyLocation::Index(1), BLUE);
    let _ = keybow.set_led(&keybow::KeyLocation::Index(2), BLUE);
    let _ = keybow.set_led(&keybow::KeyLocation::Index(3), BLUE);
    let _ = keybow.set_led(&keybow::KeyLocation::Index(4), BLUE);
    let _ = keybow.show_leds();

    let _ = handle.write_all(
        format!(
            "\nPress keys 00 to 04 in sequence: \n{}\n",
            hw_info.layout_text
        )
        .as_bytes(),
    );

    thread::sleep(Duration::from_secs(4));

    let peek_events = reg.peek_next_n_events(4).unwrap();
    let four_chars = reg.get_next_n_events_masked([true; 12], 4).unwrap();
    let fifth = reg.get_next_char().unwrap();

    assert!(peek_events[0].char == '0');
    assert!(peek_events[1].char == '1');
    assert!(peek_events[2].char == '2');
    assert!(peek_events[3].char == '3');

    assert!(four_chars[0].char == '0');
    assert!(four_chars[1].char == '1');
    assert!(four_chars[2].char == '2');
    assert!(four_chars[3].char == '3');
    assert!(fifth == '4');
}

#[test]
fn test_wait_next_char_and_leds_manually() {
    let stderr = io::stderr();
    let mut handle = stderr.lock();

    let hw_info = HardwareInfo::get_info();

    // Rewrite test if other hardware.
    assert_eq!(hw_info.num_keys, 12);
    let complete_chars = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b'];

    let _ = handle.write_all(
        format!("\nTest: press keys in sequence.\n{}\n", hw_info.layout_text,).as_bytes(),
    );

    let mut keybow = keybow::Keybow::new();

    let _ = handle.write_all(format!(
        "\nManual test: press keys in sequence.\nKey to press should be red, turn green when pressed.\n{}",
        hw_info.layout_text).as_bytes());
    let mut reg = keybow.register_events_blocking(
        [true; hw_specific::NUM_KEYS],
        [false; hw_specific::NUM_KEYS],
        20,
        complete_chars,
        Duration::MAX,
    );

    keybow.clear_leds();
    let _ = keybow.show_leds();

    for (index, one_char) in complete_chars.iter().enumerate() {
        let _ = keybow.set_led(&keybow::KeyLocation::Index(index), RED);
        let _ = keybow.show_leds();
        let read_char = reg
            .wait_next_char(Duration::MAX)
            .expect("unexpected timeout")
            .to_string();
        let read_bytes = &read_char.as_bytes();
        let _ = handle.write_all(read_bytes);
        assert_eq!(read_bytes[0], one_char.to_string().as_bytes()[0]);
        let _ = keybow.set_led(&keybow::KeyLocation::Index(index), GREEN);
        let _ = keybow.show_leds();
    }

    keybow.clear_leds();
    let _ = keybow.show_leds();

    let _ = handle.write_all(b"\n\nManual test: now press sequence BACKWARDS.\n\n");
    for (index, one_char) in complete_chars.iter().rev().enumerate() {
        let backwards_index = hw_info.num_keys - index - 1;
        let _ = keybow.set_led(&keybow::KeyLocation::Index(backwards_index), RED);
        let _ = keybow.show_leds();
        let read_char = reg
            .wait_next_char(Duration::MAX)
            .expect("unexpected timeout")
            .to_string();
        let read_bytes = &read_char.as_bytes();
        let _ = handle.write_all(read_bytes);
        assert_eq!(read_bytes[0], one_char.to_string().as_bytes()[0]);
        let _ = keybow.set_led(&keybow::KeyLocation::Index(backwards_index), GREEN);
        let _ = keybow.show_leds();
    }

    keybow.clear_leds();
    let _ = keybow.show_leds();
}
