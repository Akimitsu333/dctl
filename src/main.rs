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
    fn new(fpath: &str) -> Self {
        Self(BufReader::new(File::open(fpath).expect("bad open file")).lines())
    }
}

impl Iterator for ConfigReader {
    type Item = (String, String, Vec<String>);

    fn next(&mut self) -> Option<Self::Item> {
        for line in &mut self.0 {
            let line = line.expect("load: bad read line(of config file)");
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

            let name = parts[0];
            let command = parts[1];

            info!("load: {}: {} {}", name, command, args.join(" "));

            return Some((name.to_string(), command.to_string(), args));
        }

        None
    }
}

struct ServiceStack {
    stack: HashMap<String, ArcService>,
    // fpath: String,
}

impl Display for ServiceStack {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut status_queue: Vec<String> = Vec::new();
        for (k, v) in &self.stack {
            status_queue.push(format!("{} {}", v, k));
        }
        write!(f, "{}", status_queue.join("\n"))
    }
}

impl ServiceStack {
    fn new(stack: HashMap<String, ArcService>) -> Self {
        Self { stack }
    }

    fn init(fpath: &str) -> Self {
        let config_hashmap: HashMap<String, ArcService> = ConfigReader::new(fpath)
            .map(|(name, command, args)| (name, ArcService::new(command, args)))
            .collect();

        ServiceStack::new(config_hashmap)
    }

    fn start(&self, name: &str) -> String {
        match self.stack.get(name) {
            Some(service) => service.start().to_string(),
            None => String::from("service: can't find {name}"),
        }
    }

    fn stop(&self, name: &str) -> String {
        match self.stack.get(name) {
            Some(service) => service.stop().to_string(),
            None => String::from("service: can't find {name}"),
        }
    }

    fn restart(&self, name: &str) -> String {
        match self.stack.get(name) {
            Some(service) => service.stop().start().to_string(),
            None => String::from("service: can't find {name}"),
        }
    }

    fn status(&self, name: &str) -> String {
        match self.stack.get(name) {
            Some(service) => service.to_string(),
            None => String::from("service: can't find {name}"),
        }
    }

    fn start_all(&self) -> String {
        let _: Vec<&ArcService> = self.stack.values().map(|s| s.start()).collect();
        self.to_string()
    }

    fn stop_all(&self) -> String {
        let _: Vec<&ArcService> = self.stack.values().map(|s| s.stop()).collect();
        self.to_string()
    }
}

struct ArcService(Arc<Service>);
struct Service {
    command: String,
    args: Vec<String>,
    flag: AtomicBool,
    pid: AtomicU32,
    guardian: Mutex<Option<JoinHandle<()>>>,
}

impl Service {
    fn new(command: String, args: Vec<String>) -> Self {
        Self {
            command,
            args,
            flag: AtomicBool::new(true),
            pid: AtomicU32::new(0),
            guardian: Mutex::new(None),
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

    fn start(&self) -> &Self {
        let mut guardian = self.0.guardian.lock().unwrap();

        if guardian.is_none() {
            self.0.flag.store(true, Ordering::Relaxed);

            let service = Arc::clone(&self.0);

            *guardian = Some(thread::spawn(move || loop {
                let mut command = match Command::new(&service.command).args(&service.args).spawn() {
                    Ok(command) => command,
                    Err(_) => {
                        error!(
                            "command: bad start: {} {}",
                            &service.command,
                            service.args.join(" ")
                        );
                        *service.guardian.lock().unwrap() = None;
                        service.flag.store(false, Ordering::Release);
                        break;
                    }
                };

                service.pid.store(command.id(), Ordering::Release);

                let start_time = Instant::now();

                let result = command.wait().unwrap().success();

                service.pid.store(0, Ordering::Release);

                let time_result = start_time.elapsed() > Duration::from_secs(RESTART_SEC);
                let flag = service.flag.load(Ordering::Acquire);

                if !result && flag && time_result {
                    continue;
                } else {
                    error!(
                        "command: terminate: {} {}",
                        &service.command,
                        service.args.join(" ")
                    );
                    *service.guardian.lock().unwrap() = None;
                    service.flag.store(false, Ordering::Release);
                    break;
                }
            }));
        }

        self
    }

    fn stop(&self) -> &Self {
        let guardian = self.0.guardian.lock().unwrap();

        if guardian.is_some() {
            self.0.flag.store(false, Ordering::Relaxed);

            kill_(self.0.pid.load(Ordering::Relaxed), 15);

            self.0.pid.store(0, Ordering::Release);
        }

        self
    }
}

fn daemon() {
    let _ = SimpleLogger::init(LevelFilter::Info, LOG_PATH);

    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH).expect("socket: bad bind(path)");

    let stack = Arc::new(ServiceStack::init(CONFIG_PATH));

    info!("daemon: daemon start runining");

    let _ = stack.start_all();

    info!("service: services start running");

    for stream in listener.incoming() {
        let mut stream = stream.expect("socket: bad accept socket");

        let stack = Arc::clone(&stack);

        thread::spawn(move || {
            let mut message = String::new();
            stream
                .read_to_string(&mut message)
                .expect("message: bad read");
            let message: Vec<&str> = message.split('#').collect();

            match (message[0], message[1]) {
                ("daemon", "stop") => {
                    stream
                        .write_all(stack.stop_all().as_bytes())
                        .expect("message: bad send");

                    info!("daemon: daemon is ready to exit");

                    std::process::exit(0);
                }
                ("daemon", "status") => {
                    stream
                        .write_all(stack.to_string().as_bytes())
                        .expect("message: bad send");
                }
                ("status", name) => {
                    stream
                        .write_all(stack.status(name).as_bytes())
                        .expect("message: bad send");
                }
                ("start", name) => {
                    info!("service: start: {name}");

                    stream
                        .write_all(format!("{} {name}", stack.start(name)).as_bytes())
                        .expect("message: bad send");
                }
                ("stop", name) => {
                    info!("service: stop: {name}");

                    stream
                        .write_all(format!("{} {name}", stack.stop(name)).as_bytes())
                        .expect("message: bad send");
                }
                ("restart", name) => {
                    info!("service: restart: {name}");

                    stream
                        .write_all(format!("{} {name}", stack.restart(name)).as_bytes())
                        .expect("message: bad send");
                }
                _ => {
                    error!("option: invalid parameter");
                    stream
                        .write_all("option: invalid parameter".as_bytes())
                        .expect("message: bad send");
                }
            }

            stream.shutdown(std::net::Shutdown::Both).unwrap();
        });
    }
}

fn client(args: (&str, &str)) {
    let mut stream = UnixStream::connect(SOCKET_PATH).expect("socket: bad connect(path)");
    stream
        .write_all(format!("{}#{}", args.0, args.1).as_bytes())
        .expect("message: bad send");
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("reponse: bad read");
    println!("{}", response);
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
            eprintln!("option: bad command format");
            std::process::exit(-1);
        }
    };

    match normalized_args {
        ("daemon", "start") => daemon(),
        _ => client(normalized_args),
    }
}
