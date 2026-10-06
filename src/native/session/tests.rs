thread_local! {
    pub(in crate::native) static FIXED_UNIX_MS: std::cell::Cell<Option<u128>> = const { std::cell::Cell::new(None) };
}

pub(super) fn with_fixed_time<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(Option<u128>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FIXED_UNIX_MS.set(self.0);
        }
    }
    let _reset = Reset(FIXED_UNIX_MS.replace(Some(123456789)));
    f()
}
