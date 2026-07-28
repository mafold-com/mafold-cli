fn main() {
    // UniFFI is a native-only dependency; on wasm this bin is an empty stub.
    #[cfg(not(target_arch = "wasm32"))]
    uniffi::uniffi_bindgen_main()
}
