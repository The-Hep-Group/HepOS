#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

use hepos_std::{String, Vec, println};

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    println!("Hello from HepOS ring-3!");

    let msg = String::from("hepos-std String works");
    println!("{}", msg);

    let v: Vec<u32> = (0u32..8).map(|i| i * i).collect();
    println!("squares: {:?}", v);

    println!("PID: {}", hepos_rt::sys_getpid());

    hepos_rt::sys_exit(0);
}
