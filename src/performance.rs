//! Opt-in stage timings. No document text or paths are recorded.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn output() -> Option<&'static Mutex<File>> {
    static OUTPUT: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    OUTPUT
        .get_or_init(|| {
            let path = std::env::var_os("TEXQUILL_PROFILE")?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

pub(crate) struct Span {
    stage: &'static str,
    start: Option<Instant>,
}

impl Span {
    pub(crate) fn new(stage: &'static str) -> Self {
        Self {
            stage,
            start: output().map(|_| Instant::now()),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let (Some(start), Some(output)) = (self.start, output()) {
            if let Ok(mut file) = output.lock() {
                let _ = writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "stage": self.stage,
                        "elapsed_us": start.elapsed().as_micros(),
                        "finished_us": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_micros(),
                    })
                );
            }
        }
    }
}
