use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU32},
        Arc,
    },
    thread,
};

const BASEPATH: &str = "/data/daemon";
const SOCKPATH: &str = "/data/daemon/sock";
const AUTOSPATH: &str = "/data/daemon/auto";

extern "C" {
    fn kill(pid: u32, signal: u32) -> i32;
}

fn kill_(pid: u32) -> Result<()> {
    let result = unsafe { kill(pid, 15) };
    match result {
        0 => Ok(()),
        _ => Err(Error::Internal("Bad kill -15 service")),
    }
}

#[derive(Debug)]
enum Error {
    Dyn(String),
    Internal(&'static str),
    Exit,
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Dyn(value.to_string())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dyn(e) => write!(f, "{}", e),
            Self::Internal(e) => write!(f, "{}", e),
            Self::Exit => write!(f, "Exit"),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
struct Status {
    pid: AtomicU32,
    exit: AtomicBool,
}

impl Status {
    fn new() -> Self {
        Self {
            pid: AtomicU32::default(),
            exit: AtomicBool::default(),
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pid = self.pid.load(std::sync::atomic::Ordering::Acquire);
        let exit = match self.exit.load(std::sync::atomic::Ordering::Acquire) {
            false => "*",
            true => "",
        };
        write!(f, "{} [{}]", pid, exit)
    }
}

fn load(name: &str) -> Result<Vec<String>> {
    let path = format!("{}/{name}/default.service", BASEPATH);
    let service_file = File::open(path)?;

    let reader = BufReader::new(service_file);
    let mut command = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            break;
        }

        command.push(line.to_string())
    }

    Ok(command)
}

fn stop(service_stack: &mut HashMap<String, Arc<Status>>, name: &str) -> Result<()> {
    let status = service_stack
        .remove(name)
        .ok_or(Error::Internal("Bad find service"))?;
    let pid = status.pid.load(std::sync::atomic::Ordering::Acquire);

    if pid != 0 {
        status
            .exit
            .store(true, std::sync::atomic::Ordering::Release);
        kill_(pid)?;
    }

    Ok(())
}

fn start(service_stack: &mut HashMap<String, Arc<Status>>, name: &str) -> Result<()> {
    let name = name.to_string();
    let name_c = name.clone();
    let status = Arc::new(Status::new());
    let status_c = status.clone();

    if service_stack.contains_key(&name) {
        stop(service_stack, &name)?;
    }

    service_stack.insert(name.clone(), status);

    let _ = thread::spawn(|| -> Result<()> {
        let name = name_c;
        let mut service = load(&name)?;
        let name = service.remove(0);
        let status = status_c;

        loop {
            let mut handle = Command::new(&name).args(&service).env_clear().spawn()?;

            status
                .pid
                .store(handle.id(), std::sync::atomic::Ordering::Release);

            if handle.wait()?.success() {
                status.pid.store(0, std::sync::atomic::Ordering::Release);
                status
                    .exit
                    .store(true, std::sync::atomic::Ordering::Release);
                break;
            }

            if status.exit.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
        }

        Ok(())
    });

    Ok(())
}

fn status(
    service_stack: &mut HashMap<String, Arc<Status>>,
    name: &str,
    mut stream: UnixStream,
) -> Result<()> {
    let status = service_stack
        .get(name)
        .ok_or(Error::Internal("Bad find service"))?;
    let message = format!("{name} {status}");
    stream.write_all(message.as_bytes())?;

    Ok(())
}

fn status_all(
    service_stack: &mut HashMap<String, Arc<Status>>,
    mut stream: UnixStream,
) -> Result<()> {
    let status = service_stack
        .iter()
        .map(|(name, status)| format!("{name} {status}"))
        .collect::<Vec<String>>();
    let message = status.join("\n");
    stream.write_all(message.as_bytes())?;

    Ok(())
}

fn auto_start(service_stack: &mut HashMap<String, Arc<Status>>) -> Result<()> {
    let mut services = File::open(AUTOSPATH)?;
    let mut buffer = String::new();

    services.read_to_string(&mut buffer)?;
    let services = buffer.split_whitespace().collect::<Vec<&str>>();

    for name in services {
        start(service_stack, name)?;
    }

    Ok(())
}

fn daemon_exec(
    mut stream: UnixStream,
    buffer: &mut String,
    service_stack: &mut HashMap<String, Arc<Status>>,
) -> Result<()> {
    buffer.clear();
    stream.read_to_string(buffer)?;
    let signal = buffer
        .split_once('/')
        .ok_or(Error::Internal("Bad parse signal"))?;

    match signal {
        ("daemon", "stop") => Err(Error::Exit),
        ("daemon", "status") => status_all(service_stack, stream),
        ("status", name) => status(service_stack, name, stream),
        ("start", name) => start(service_stack, name),
        ("stop", name) => stop(service_stack, name),
        _ => stream
            .write_all("Invalid parameter".as_bytes())
            .or(Err(Error::Internal("Bad parameter"))),
    }
}

fn daemon() -> Result<()> {
    let _ = std::fs::remove_file(SOCKPATH);
    let listener = UnixListener::bind(SOCKPATH)?;
    let mut buffer = String::with_capacity(1024);
    let mut service_stack = HashMap::new();

    auto_start(&mut service_stack)?;

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("{}", e);
                continue;
            }
        };

        let result = daemon_exec(stream, &mut buffer, &mut service_stack);

        match result {
            Err(Error::Exit) => break,
            Err(e) => eprintln!("{}", e),
            _ => (),
        };
    }

    Ok(())
}

fn client(arg_1: &str, arg_2: &str) -> Result<()> {
    let mut stream = UnixStream::connect(SOCKPATH)?;
    stream.write_all(format!("{arg_1}/{arg_2}").as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    println!("{}", response);

    Ok(())
}

fn main() -> Result<()> {
    /*
        解析命令参数
    */
    let args: Vec<String> = std::env::args().collect();

    let (arg_1, arg_2) = match args.len() {
        3 => (args[1].as_str(), args[2].as_str()),
        2 => ("daemon", args[1].as_str()),
        1 => return daemon(),
        _ => return Err(Error::Internal("Invalid format")),
    };

    client(arg_1, arg_2)
}
