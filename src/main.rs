use libc::kill_;
use log::{error, info, LevelFilter, Metadata, Record};
use settings::*;
use std::collections::HashMap;
use std::fmt::{self, Display};
use std::io::prelude::*;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const RESTART_SEC: u64 = 1;

#[cfg(target_os = "linux")]
mod settings {
    pub const SOCKET_PATH: &str = "/tmp/daemon.sock";
    pub const CONFIG_PATH: &str = "/tmp/config";
    pub const LOG_PATH: &str = "/tmp/daemon.log";
}

#[cfg(target_os = "android")]
mod settings {
    pub const SOCKET_PATH: &str = "/data/daemon/daemon.sock";
    pub const CONFIG_PATH: &str = "/data/daemon/config";
    pub const LOG_PATH: &str = "/data/daemon/daemon.log";
}

mod libc {
    extern "C" {
        fn kill(pid: u32, sig: u32) -> i32;
    }

    pub fn kill_(pid: u32, sig: u32) -> i32 {
        unsafe { kill(pid, sig) }
    }
}

struct SimpleLogger {
    level: LevelFilter,
    writable: Mutex<std::fs::File>,
}

impl SimpleLogger {
    fn init(level: LevelFilter, path: &str) -> Result<(), log::SetLoggerError> {
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

struct ServiceQueue {
    queue: HashMap<String, Service>,
}

impl std::ops::Deref for ServiceQueue {
    type Target = HashMap<String, Service>;

    fn deref(&self) -> &Self::Target {
        &self.queue
    }
}

impl std::ops::DerefMut for ServiceQueue {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.queue
    }
}

impl Display for ServiceQueue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut status_queue: Vec<String> = Vec::new();
        for (k, v) in &self.queue {
            status_queue.push(format!("{} {}", v, k));
        }
        write!(f, "{}", status_queue.join("\n"))
    }
}

impl ServiceQueue {
    fn new(config_path: &str) -> Self {
        let mut queue = HashMap::new();

        info!("[load] start loading the service");

        for line in
            std::io::BufReader::new(std::fs::File::open(config_path).expect("bad open file"))
                .lines()
        {
            let line = line.expect("[load] bad read line(of config file)");

            let mut parts: Vec<&str> = Vec::new();
            let mut args: Vec<String> = Vec::new();

            match line.chars().filter(|&c| c == ' ').count() {
                0 => continue,
                1 => {
                    parts.extend(line.splitn(2, ' ').collect::<Vec<&str>>());
                }
                _ => {
                    parts.extend(line.splitn(3, ' ').collect::<Vec<&str>>());
                    args.extend(
                        parts[2]
                            .split_whitespace()
                            .map(|s| s.to_string())
                            .collect::<Vec<String>>(),
                    );
                }
            }

            info!("[load] {1} {2} ({0})", parts[0], parts[1], args.join(" "));

            queue.insert(
                parts[0].to_string(),
                Service::new(parts[1].to_string(), args),
            );
        }

        Self { queue }
    }

    fn start(&self) {
        for v in self.queue.values() {
            v.start();
        }
    }

    fn stop(&self) {
        for v in self.queue.values() {
            v.stop();
        }
    }
}

struct ServiceStatus {
    flag: AtomicBool,
    pid: AtomicU32,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceStatus {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(true),
            pid: AtomicU32::new(0),
            thread: Mutex::new(None),
        }
    }

    fn get(&self) -> (String, String) {
        (
            self.flag.load(Ordering::Relaxed).to_string(),
            self.pid.load(Ordering::Relaxed).to_string(),
        )
    }
}

struct Service {
    command: Arc<(String, Vec<String>)>,
    status: Arc<ServiceStatus>,
}

impl Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let status = self.status.get();
        write!(f, "[{}] {}", status.0, status.1)
    }
}

impl Service {
    fn new(basename: String, args: Vec<String>) -> Self {
        Self {
            command: Arc::new((basename, args)),
            status: Arc::new(ServiceStatus::new()),
        }
    }

    fn start(&self) {
        let mut thread = self.status.thread.lock().unwrap();

        let command = Arc::clone(&self.command);
        let status = Arc::clone(&self.status);

        if thread.is_none() {
            self.status.flag.store(true, Ordering::Relaxed);

            *thread = Some(thread::spawn(move || loop {
                let mut command = Command::new(&command.0)
                    .args(&command.1)
                    .spawn()
                    .expect("[command] bad start(wrong command)");

                status.pid.store(command.id(), Ordering::Release);

                let start_time = Instant::now();

                let result = command.wait().unwrap().success();

                status.pid.store(0, Ordering::Release);

                let time_result = start_time.elapsed() > Duration::from_secs(RESTART_SEC);
                let flag = status.flag.load(Ordering::Acquire);

                if !result && flag && time_result {
                    continue;
                } else {
                    error!("[command] terminate");
                    *status.thread.lock().unwrap() = None;
                    status.flag.store(false, Ordering::Release);
                    break;
                }
            }));
        }
    }

    fn stop(&self) {
        let thread = self.status.thread.lock().unwrap();

        if thread.is_some() {
            self.status.flag.store(false, Ordering::Relaxed);

            kill_(self.status.pid.load(Ordering::Relaxed), 15);

            self.status.pid.store(0, Ordering::Release);
        }
    }

    fn restart(&self) {
        self.stop();
        self.start();
    }
}

fn main() {
    /*
        解析命令参数
    */
    let args: Vec<String> = std::env::args().collect();

    let normalized_args = match args.len() {
        1 => ("daemon", "start"),
        2 => ("daemon", args[1].as_str()),
        3 => (args[1].as_str(), args[2].as_str()),
        _ => {
            eprintln!("[option] bad command format");
            std::process::exit(-1);
        }
    };

    match normalized_args {
        ("daemon", "start") => {
            let _ = SimpleLogger::init(LevelFilter::Info, LOG_PATH);

            let _ = std::fs::remove_file(SOCKET_PATH);
            let listener = UnixListener::bind(SOCKET_PATH).expect("[socket] bad bind(path)");

            let queue = Arc::new(ServiceQueue::new(CONFIG_PATH));

            info!("[daemon] daemon start runining");

            queue.start();

            info!("[service] services start running");

            for stream in listener.incoming() {
                let mut stream = stream.expect("[socket] bad unwrap unix stream");

                let queue = Arc::clone(&queue);

                thread::spawn(move || {
                    let mut message = String::new();
                    stream
                        .read_to_string(&mut message)
                        .expect("[message] bad read");
                    let message: Vec<&str> = message.split('#').collect();

                    match (message[0], message[1]) {
                        ("daemon", "stop") => {
                            queue.stop();
                            stream
                                .write_all(queue.to_string().as_bytes())
                                .expect("[message] bad send");

                            info!("[daemon] daemon is ready to exit");

                            std::process::exit(0);
                        }
                        ("daemon", "status") => {
                            stream
                                .write_all(queue.to_string().as_bytes())
                                .expect("[message] bad send");
                        }
                        ("status", s_name) => {
                            let service = queue.get(s_name).unwrap();
                            stream
                                .write_all(service.to_string().as_bytes())
                                .expect("[message] bad send");
                        }
                        ("restart", s_name) => {
                            info!("[service] [restart] {s_name}");

                            let service = queue.get(s_name).unwrap();
                            service.restart();
                            stream
                                .write_all(format!("{service} {s_name}").as_bytes())
                                .expect("[message] bad send");
                        }
                        ("start", s_name) => {
                            info!("[service] [start] {s_name}");

                            let service = queue.get(s_name).unwrap();
                            service.start();
                            stream
                                .write_all(format!("{service} {s_name}").as_bytes())
                                .expect("[message] bad send");
                        }
                        ("stop", s_name) => {
                            info!("[service] [stop] {s_name}");

                            let service = queue.get(s_name).unwrap();
                            service.stop();
                            stream
                                .write_all(format!("{service} {s_name}").as_bytes())
                                .expect("[message] bad send");
                        }
                        _ => {
                            error!("[option] invalid parameter");
                            stream
                                .write_all("[option] invalid parameter".as_bytes())
                                .expect("[message] bad send");
                        }
                    }

                    stream.shutdown(std::net::Shutdown::Both).unwrap();
                });
            }
        }
        _ => {
            let mut stream = UnixStream::connect(SOCKET_PATH).expect("[socket] bad connect(path)");
            stream
                .write_all(format!("{}#{}", normalized_args.0, normalized_args.1).as_bytes())
                .expect("[message] bad send");
            stream.shutdown(std::net::Shutdown::Write).unwrap();

            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("[reponse] bad read");
            println!("{}", response);
        }
    }
}
