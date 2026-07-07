use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

const STACK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskState { Running, Ready, Blocked, Dead }

pub struct Task {
    pub id:      usize,
    pub name:    &'static str,
    pub state:   TaskState,
    pub wake_at: u64,         // valid only while state == Blocked; in TICK_COUNT units
    _stack:      Vec<u8>,       // keeps the stack allocation alive
    pub rsp:     u64,
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
        //   [RSP+40] = rbx     ← entry fn ptr, consumed by task_trampoline
        //   [RSP+48] = return address  ← ret jumps here (task_trampoline)
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
            f.add(5).write(entry as *const () as u64); // rbx — entry, read by trampoline
            f.add(6).write(task_trampoline as *const () as u64); // ret addr
        }

        Task { id, name, state: TaskState::Ready, wake_at: 0, _stack: stack, rsp: rsp as u64 }
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
    ///
    /// The outgoing task (`self.current`) is only reset to `Ready` if it's
    /// still `Running` — i.e. this is an ordinary timer-driven preemption.
    /// `exit_current()`/`sleep_ms()` set a different terminal state (`Dead`/
    /// `Blocked`) on themselves *before* calling this, and that must survive
    /// the switch untouched — overwriting it back to `Ready` here would undo
    /// the exit/sleep the instant it took effect.
    pub fn next(&mut self) -> Option<(*mut u64, u64)> {
        let n = self.tasks.len();
        if n < 2 { return None; }

        // Blocked tasks whose wake time has passed become eligible again.
        let now = TICK_COUNT.load(Ordering::Relaxed);
        for t in self.tasks.iter_mut() {
            if t.state == TaskState::Blocked && now >= t.wake_at {
                t.state = TaskState::Ready;
            }
        }

        let mut next = (self.current + 1) % n;
        for _ in 0..n {
            if self.tasks[next].state == TaskState::Ready { break; }
            next = (next + 1) % n;
        }
        if self.tasks[next].state != TaskState::Ready { return None; }
        if next == self.current { return None; }

        if self.tasks[self.current].state == TaskState::Running {
            self.tasks[self.current].state = TaskState::Ready;
        }
        self.tasks[next].state = TaskState::Running;

        let old_rsp = &mut self.tasks[self.current].rsp as *mut u64;
        let new_rsp = self.tasks[next].rsp;
        self.current = next;
        Some((old_rsp, new_rsp))
    }
}

pub static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::empty());

// IDs 0/1 are the boot-registered idle/blink tasks (kmain, see main.rs).
static NEXT_TASK_ID: AtomicUsize = AtomicUsize::new(2);

/// Spawn a new task at runtime. Reuses a `Dead` task's slot (and its stack
/// allocation) if one exists — same "evict an exited entry" pattern
/// `process::exec()` uses for its process table — otherwise appends a new
/// slot. Returns the new task's id.
pub fn spawn(name: &'static str, entry: fn() -> !) -> usize {
    let id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let task = Task::new(id, name, entry);
    let mut sched = SCHEDULER.lock();
    if let Some(slot) = sched.tasks.iter_mut().find(|t| t.state == TaskState::Dead) {
        *slot = task;
    } else {
        sched.tasks.push(task);
    }
    id
}

/// Terminate the calling task. Marks it `Dead` (its slot is reused by a
/// future `spawn()`, freeing the old stack) and switches away — never
/// returns. If no other task is currently `Ready`/`Blocked`-with-work, this
/// is the last runnable task and the CPU halts.
pub fn exit_current() -> ! {
    let switch = {
        let mut sched = SCHEDULER.lock();
        let cur = sched.current;
        sched.tasks[cur].state = TaskState::Dead;
        sched.next()
    };
    if let Some((old_rsp, new_rsp)) = switch {
        unsafe { context_switch(old_rsp, new_rsp); }
    }
    loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
}

/// Block the calling task for approximately `ms` milliseconds (rounded up to
/// the nearest timer tick, ~10ms — see `TICK_COUNT`'s doc comment) and yield
/// to another task. If no other task is `Ready` right now, falls straight
/// through without actually blocking (a single-task system can't afford to
/// sleep — there'd be nothing to wake it).
pub fn sleep_ms(ms: u64) {
    let ticks = (ms / 10).max(1);
    let switch = {
        let mut sched = SCHEDULER.lock();
        let cur = sched.current;
        let wake_at = TICK_COUNT.load(Ordering::Relaxed) + ticks;
        sched.tasks[cur].wake_at = wake_at;
        sched.tasks[cur].state = TaskState::Blocked;
        sched.next()
    };
    if let Some((old_rsp, new_rsp)) = switch {
        unsafe { context_switch(old_rsp, new_rsp); }
    } else {
        // No other task to run — undo the Blocked state we tentatively set,
        // since nothing will ever call next() to promote us back to Ready.
        let mut sched = SCHEDULER.lock();
        let cur = sched.current;
        sched.tasks[cur].state = TaskState::Running;
    }
}

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

/// Entered via `ret` from `context_switch` the first time a task runs — never
/// via `iretq`. Two things the normal resume path (tick() returning into
/// timer_stub's "call eoi; iretq") would have done never happen here, because
/// this `ret` jumps straight into the new task instead of unwinding back
/// through tick()/timer_stub:
///   1. EOI for the interrupt that caused this very switch is never sent —
///      the in-service bit for the timer vector stays set forever, so the
///      LAPIC blocks all future timer interrupts. Without this, the very
///      first task switch permanently kills the timer (TICK_COUNT freezes).
///   2. RFLAGS.IF is never restored — it's whatever it was inside the timer
///      ISR (0), so the new task would otherwise run with interrupts
///      permanently off.
/// `sti`'s one-instruction shadow covers the `jmp`, so IF only takes effect
/// once the real entry point (left in rbx by `Task::new`) starts running.
#[unsafe(naked)]
unsafe extern "C" fn task_trampoline() {
    core::arch::naked_asm!(
        "call {eoi}",
        "sti",
        "jmp rbx",
        eoi = sym crate::apic::eoi,
    );
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
