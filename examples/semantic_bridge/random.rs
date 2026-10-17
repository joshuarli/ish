pub(crate) struct Generator {
    state: u64,
}

impl Generator {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    pub(crate) fn below(&mut self, upper: u64) -> u64 {
        self.next() % upper
    }
}
