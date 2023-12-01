extern "C" {
    fn kill(pid: u32, sig: u32) -> i32;
}

pub fn kill_(pid: u32, sig: u32) -> i32 {
    unsafe { kill(pid, sig) }
}
