#[cfg(test)]
const ONLY_IN_TESTS: u32 = 1;

static AFTER_THE_TEST_CONST: std::sync::Mutex<u32> = std::sync::Mutex::new(0);
