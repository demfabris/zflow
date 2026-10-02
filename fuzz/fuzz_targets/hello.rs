#![no_main]

use libfuzzer_sys::fuzz_target;
use zflow::wire::{Family, decode_family};

// Anyone on the network can send a hello before either side trusts the other.
fuzz_target!(|data: &[u8]| {
    let _ = decode_family(data, Family::Hello);
});
