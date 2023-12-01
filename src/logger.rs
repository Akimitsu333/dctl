use log::{LevelFilter, Metadata, Record};
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

pub struct SimpleLogger {
    level: LevelFilter,
    writable: Mutex<File>,
}

impl SimpleLogger {
    pub fn init(level: LevelFilter, path: &str) -> Result<(), log::SetLoggerError> {
        log::set_max_level(level);
        log::set_boxed_logger(SimpleLogger::new(
            level,
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("[log] bad open file"),
        ))
    }

    fn new(level: LevelFilter, writable: std::fs::File) -> Box<SimpleLogger> {
        Box::new(SimpleLogger {
            level,
            writable: Mutex::new(writable),
        })
    }
}

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let mut writable = self.writable.lock().unwrap();
            let _ = writable.write_all(
                format!(
                    "[{}] {}\n",
                    record.level().as_str().to_lowercase(),
                    record.args()
                )
                .as_bytes(),
            );
        }
    }

    fn flush(&self) {
        let _ = self.writable.lock().unwrap().flush();
    }
}
