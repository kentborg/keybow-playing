/** Prints to stderr, does not append a new line, and immediately
 * flushes output.
 *
 * This is the debug version, there is a no-op non-debug version,
 * too. To build with non-debug version do not build with `--features
 * debug_waveform_print`.
 */
#[cfg(feature = "debug_waveform_print")]
#[macro_export]
macro_rules! debug_waveform {
    ($( $args:expr ),*) => { eprint!( $( $args ),* ); let _ = std::io::Write::flush(&mut std::io::stderr()); }
}

/** No-op, non-debug version of the macro.
 *
 *  The other, debug, version prints to stderr, does not append a new
 *  line, and immediately flushes output. It is used to see ASCII-ish
 *  "waveforms" of the keybounce events and how the code is handling
 *  them.
 *
 *   To run the debug version:
 *
 * ```
 *   cargo run --features debug_waveform_print
 * ```
 */
#[macro_export]
#[cfg(not(feature = "debug_waveform_print"))]
macro_rules! debug_waveform {
    ($( $args:expr ),*) => {};
}
