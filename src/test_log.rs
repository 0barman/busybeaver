use std::sync::{Mutex, Once};

struct CapturingLogger {
    messages: Mutex<Vec<String>>,
}

impl log::Log for CapturingLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.target() == "busybeaver"
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let mut messages = self
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            messages.push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

static LOGGER: CapturingLogger = CapturingLogger {
    messages: Mutex::new(Vec::new()),
};
static INSTALL: Once = Once::new();

pub(crate) fn init() {
    INSTALL.call_once(|| {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
    });
}

pub(crate) fn contains(code: &str) -> bool {
    LOGGER
        .messages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|message| message.contains(code))
}
