#[cfg(all(test, unix))]
mod unix_tests {
    static IN_UNIX_TESTS: std::sync::Mutex<u32> = std::sync::Mutex::new(0);
}

#[cfg(any(test, unix))]
static ANY_TEST_OR_UNIX: std::sync::Mutex<u32> = std::sync::Mutex::new(0);

#[cfg(any(test, all(test, unix)))]
static ANY_ARM_TEST: std::sync::Mutex<u32> = std::sync::Mutex::new(0);

#[cfg(not(test))]
static NOT_TEST: std::sync::Mutex<u32> = std::sync::Mutex::new(0);

#[test]
fn a_bare_test() {
    static IN_A_BARE_TEST: std::sync::Mutex<u32> = std::sync::Mutex::new(0);
}
