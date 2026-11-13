#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    multiview_h3::testing::capsule(data);
});
