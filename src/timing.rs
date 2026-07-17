use std::time::Instant;

pub struct Timings {
    enabled: bool,
    start: Instant,
    stages: Vec<(&'static str, f64)>,
}

impl Timings {
    pub fn new() -> Self {
        Self {
            enabled: std::env::var_os("GLEP_TIMING").is_some(),
            start: Instant::now(),
            stages: Vec::new(),
        }
    }
    pub fn stage(&mut self, name: &'static str) {
        if self.enabled {
            let now = Instant::now();
            let ms = now.duration_since(self.start).as_secs_f64() * 1000.0;
            self.stages.push((name, ms));
            self.start = now;
        }
    }
    pub fn finish(&self) {
        if self.enabled {
            let mut total = 0.0;
            for (name, ms) in &self.stages {
                eprintln!("glep timing: {name} {ms:.1}ms");
                total += ms;
            }
            eprintln!("glep timing: total {total:.1}ms");
        }
    }
}
