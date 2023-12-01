use log::{error, info, LevelFilter};
use std::collections::HashMap;
use std::fmt::{self, Display};
use std::fs::File;
use std::io::{prelude::*, BufReader, Lines};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

mod config;
mod libc;
mod logger;

use config::*;
use libc::kill_;
use logger::SimpleLogger;

struct ConfigReader(Lines<BufReader<File>>);

impl ConfigReader {
    fn new(config_path: &str) -> Self {
        Self(BufReader::new(File::open(config_path).expect("bad open file")).lines())
    }
}

impl Iterator for ConfigReader {
    type Item = (String, String, Vec<String>);

    fn next(&mut self) -> Option<Self::Item> {
        for line in &mut self.0 {
            let line = line.expect("[load] bad read line(of config file)");
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            let mut args = Vec::new();
            match parts.len() {
                0 | 1 => continue,
                2 => (),
                _ => {
                    args.extend(
                        parts[2]
                            .split_whitespace()
                            .map(|arg| arg.to_string())
                            .collect::<Vec<String>>(),
                    );
                }
            };

            return Some((parts[0].to_string(), parts[1].to_string(), args));
        }

        None
    }
}

struct ServiceStack(HashMap<String, ArcService>);

impl std::ops::Deref for ServiceStack {
    type Target = HashMap<String, ArcService>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ServiceStack {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Display for ServiceStack {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut status_queue: Vec<String> = Vec::new();
        for (k, v) in &self.0 {
            status_queue.push(format!("{} {}", v, k));
        }
        write!(f, "{}", status_queue.join("\n"))
    }
}

impl ServiceStack {
    fn new(config_path: &str) -> Self {
        let mut queue = HashMap::new();
        let config = ConfigReader::new(config_path);

        info!("[load] start loading the service");

        for (name, command, args) in config {
            info!("[load] {} {} ({})", &command, args.join(" "), &name);
            queue.insert(name, ArcService::new(command, args));
        }

        Self(queue)
    }

    fn start(&self) {
        for v in self.0.values() {
            v.start();
        }
    }

    fn stop(&self) {
        for v in self.0.values() {
            v.stop();
        }
    }
}

struct ArcService(Arc<Service>);
struct Service {
    command: String,
    args: Vec<String>,
    flag: AtomicBool,
    pid: AtomicU32,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Service {
    fn new(command: String, args: Vec<String>) -> Self {
        Self {
            command,
            args,
            flag: AtomicBool::new(true),
            pid: AtomicU32::new(0),
            thread: Mutex::new(None),
        }
    }
}

impl Display for ArcService {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let flag = self.0.flag.load(Ordering::Relaxed).to_string();
        let pid = self.0.pid.load(Ordering::Relaxed).to_string();
        write!(f, "[{}] {}", flag, pid)
    }
}

impl ArcService {
    fn new(command: String, args: Vec<String>) -> Self {
        Self(Arc::new(Service::new(command, args)))
    }

    fn start(&self) {
        let mut thread = self.0.thread.lock().unwrap();
        let service = Arc::clone(&self.0);

        if thread.is_none() {
            self.0.flag.store(true, Ordering::Relaxed);

            *thread = Some(thread::spawn(move || loop {
                let mut command = Command::new(&service.command)
                    .args(&service.args)
                    .spawn()
                    .expect("[command] bad start(wrong command)");

                service.pid.store(command.id(), Ordering::Release);

                let start_time = Instant::now();

                let result = command.wait().unwrap().success();

                service.pid.store(0, Ordering::Release);

                let time_result = start_time.elapsed() > Duration::from_secs(RESTART_SEC);
                let flag = service.flag.load(Ordering::Acquire);

                if !result && flag && time_result {
                    continue;
                } else {
                    error!("[command] terminate");
                    *service.thread.lock().unwrap() = None;
                    service.flag.store(false, Ordering::Release);
                    break;
                }
            }));
        }
    }

    fn stop(&self) {
        let thread = self.0.thread.lock().unwrap();

        if thread.is_some() {
            self.0.flag.store(false, Ordering::Relaxed);

            kill_(self.0.pid.load(Ordering::Relaxed), 15);

            self.0.pid.store(0, Ordering::Release);
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

            let queue = Arc::new(ServiceStack::new(CONFIG_PATH));

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
