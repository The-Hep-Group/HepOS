use alloc::vec::Vec;
use spin::Mutex;

const STACK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskState { Running, Ready, Dead }

pub struct Task {
    pub id:    usize,
    pub name:  &'static str,
    pub state: TaskState,
    _stack:    Vec<u8>,       // keeps the stack allocation alive
    pub rsp:   u64,
}

impl Task {
    pub fn new(id: usize, name: &'static str, entry: fn() -> !) -> Self {
        let mut stack = Vec::with_capacity(STACK_SIZE);
        stack.resize(STACK_SIZE, 0u8);

        let stack_top = stack.as_ptr() as usize + STACK_SIZE;

        // Layout that context_switch expects on the new task's stack:
        //   [RSP+0 ] = r15
        //   [RSP+8 ] = r14
        //   [RSP+16] = r13
        //   [RSP+24] = r12
        //   [RSP+32] = rbp
        //   [RSP+40] = rbx
        //   [RSP+48] = return address  ← ret jumps here
        //
        // After ret RSP = initial_rsp + 56.
        // For RSP+56 % 16 == 8 (ABI entry alignment), we need initial_rsp % 16 == 0.
        let rsp = (stack_top - 56) & !0xF;

        unsafe {
            let f = rsp as *mut u64;
            f.add(0).write(0); // r15
            f.add(1).write(0); // r14
            f.add(2).write(0); // r13
            f.add(3).write(0); // r12
            f.add(4).write(0); // rbp
            f.add(5).write(0); // rbx
            f.add(6).write(entry as *const () as u64); // ret addr
        }

        Task { id, name, state: TaskState::Ready, _stack: stack, rsp: rsp as u64 }
    }
}

pub struct Scheduler {
    pub tasks:   Vec<Task>,
    pub current: usize,
}

impl Scheduler {
    pub const fn empty() -> Self {
        Scheduler { tasks: Vec::new(), current: 0 }
    }

    pub fn add(&mut self, task: Task) {
        self.tasks.push(task);
    }

    /// Returns (old_rsp_ptr, new_rsp) without holding the lock during switch.
    pub fn next(&mut self) -> Option<(*mut u64, u64)> {
        let n = self.tasks.len();
        if n < 2 { return None; }

        let mut next = (self.current + 1) % n;
        for _ in 0..n {
            if self.tasks[next].state == TaskState::Ready { break; }
            next = (next + 1) % n;
        }
        if self.tasks[next].state != TaskState::Ready { return None; }
        if next == self.current { return None; }

        self.tasks[self.current].state = TaskState::Ready;
        self.tasks[next].state = TaskState::Running;

        let old_rsp = &mut self.tasks[self.current].rsp as *mut u64;
        let new_rsp = self.tasks[next].rsp;
        self.current = next;
        Some((old_rsp, new_rsp))
    }
}

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::empty());

/// Wall-time tick counter — incremented every ~10 ms by the APIC timer ISR.
/// Safe to poll from any context without MMIO or TSC calibration.
pub static TICK_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Called from timer ISR. Drops the lock BEFORE switching stacks.
pub fn tick() {
    TICK_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // Acquire lock, compute switch, then DROP lock before context_switch.
    let switch = SCHEDULER.lock().next();
    if let Some((old_rsp, new_rsp)) = switch {
        // Lock is released here — safe to switch stacks.
        unsafe { context_switch(old_rsp, new_rsp); }
    }
}

#[unsafe(naked)]
unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",   // save current RSP → *old_rsp
        "mov rsp, rsi",     // load new RSP
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}
