pub fn work() -> u32 {
    #[cfg(test)]
    let probe = 1;
    #[cfg(test)]
    {
        let _ = probe;
    }
    2
}

static AFTER_THE_STATEMENT: std::sync::Mutex<u32> = std::sync::Mutex::new(0);
