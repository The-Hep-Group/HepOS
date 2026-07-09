use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

const STACK_SIZE: usize = 64 * 1024;

/// Size of each task's *own* syscall/interrupt-entry kernel stack — see
/// `Task::kstack`'s doc comment for why every task needs one of these now.
/// Matches `syscall.rs`'s old one-size-fits-all `KSTACK_SIZE`.
const KSTACK_SIZE: usize = 4 * 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskState { Running, Ready, Blocked, Dead }

pub struct Task {
    pub id:      usize,
    pub name:    &'static str,
    pub state:   TaskState,
    pub wake_at: u64,         // valid only while state == Blocked; in TICK_COUNT units
    /// Set while `state == Blocked` by `block_on_irq()` — which interrupt
    /// vector this task is waiting on. `None` for an ordinary `sleep_ms()`
    /// block (time-based, woken by `next()`'s own `wake_at` check) or any
    /// non-blocked task. A task blocked this way is *not* time-woken — only
    /// `wake_irq_waiters(vector)` (called from that interrupt's handler)
    /// promotes it back to `Ready`, so `wake_at` is set to `u64::MAX` here
    /// to keep it out of the ordinary sleep-expiry check entirely.
    pub waiting_irq: Option<u8>,
    /// This task's own top-level page table (CR3 value). Every task used to
    /// implicitly share one CR3 — safe only because at most one process
    /// (with its own private low-half page tables) was ever "in flight" at
    /// a time. Now that multiple ring-3 processes can run concurrently (see
    /// `process.rs`), each task with its own address space needs its CR3
    /// restored on every resume, same as its RSP already is. Ordinary tasks
    /// (idle/blink/etc.) all default to the kernel's own root PML4, read
    /// once at creation time, and never change it.
    pub cr3:     u64,
    _stack:      Vec<u8>,       // keeps the stack allocation alive
    pub rsp:     u64,
    /// This task's own dedicated stack for handling a SYSCALL or a hardware
    /// interrupt that fires while this task is in ring 3 — a real,
    /// previously-latent bug this uncovered: both `syscall.rs`'s
    /// `PERCPU.kernel_stack` (used by the `SYSCALL` entry stub) and the
    /// TSS's `RSP0` (used when a hardware interrupt fires while in ring 3)
    /// used to be ONE GLOBAL BUFFER shared by every process — invisible
    /// with only one process ever running, since nothing else could ever
    /// be using it at the same time. Once a second concurrent process
    /// (RTL8139 + HDA's driver processes running together) started making
    /// syscalls too, a task blocked mid-syscall (e.g. inside `SYS_WAIT_IRQ`,
    /// which itself calls `block_on_irq()` — a context switch *while still
    /// executing on that shared stack*, since `SYS_WAIT_IRQ` doesn't return
    /// to ring 3 before blocking) left its saved RSP pointing *into* that
    /// shared buffer; the next task's syscall/interrupt-in-ring-3 then
    /// silently overwrote the same memory from the top, corrupting the
    /// first task's suspended state — the ELF-loading page fault this was
    /// found chasing down. Fixed by giving every task its own dedicated
    /// buffer and repointing both `PERCPU.kernel_stack` and TSS.RSP0 at the
    /// *current* task's own copy on every switch (see `next()` below and
    /// each of its four call sites).
    _kstack:     Vec<u8>,
    pub kstack_top: u64,
}

fn read_cr3() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) v, options(nostack, nomem)); }
    v
}

unsafe fn write_cr3(phys: u64) {
    core::arch::asm!("mov cr3, {}", in(reg) phys, options(nostack, nomem));
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

        let mut kstack = Vec::with_capacity(KSTACK_SIZE);
        kstack.resize(KSTACK_SIZE, 0u8);
        let kstack_top = kstack.as_ptr() as u64 + KSTACK_SIZE as u64;

        Task {
            id, name, state: TaskState::Ready, wake_at: 0, waiting_irq: None,
            cr3: read_cr3(), _stack: stack, rsp: rsp as u64,
            _kstack: kstack, kstack_top,
        }
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

    /// Returns (old_rsp_ptr, new_rsp, new_cr3, new_kstack_top) without
    /// holding the lock during the switch — the caller must `write_cr3()`
    /// and repoint the syscall/interrupt kernel stack (`syscall::
    /// set_kernel_stack()` + `gdt::set_tss_rsp0()`) at `new_kstack_top`
    /// *before* calling `context_switch()` (that part doesn't touch any of
    /// this itself; see each call site).
    ///
    /// The outgoing task (`self.current`) is only reset to `Ready` if it's
    /// still `Running` — i.e. this is an ordinary timer-driven preemption.
    /// `exit_current()`/`sleep_ms()` set a different terminal state (`Dead`/
    /// `Blocked`) on themselves *before* calling this, and that must survive
    /// the switch untouched — overwriting it back to `Ready` here would undo
    /// the exit/sleep the instant it took effect.
    pub fn next(&mut self) -> Option<(*mut u64, u64, u64, u64)> {
        let n = self.tasks.len();
        if n < 2 { return None; }

        // Blocked tasks whose wake time has passed become eligible again —
        // but not one blocked on an IRQ instead (`waiting_irq.is_some()`):
        // that one is only ever woken by `wake_irq_waiters()`, and its
        // `wake_at` is deliberately `u64::MAX` so it'd never match here
        // anyway, but the explicit check keeps this loop's own logic honest.
        let now = TICK_COUNT.load(Ordering::Relaxed);
        for t in self.tasks.iter_mut() {
            if t.state == TaskState::Blocked && t.waiting_irq.is_none() && now >= t.wake_at {
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
        let new_cr3 = self.tasks[next].cr3;
        let new_kstack_top = self.tasks[next].kstack_top;
        self.current = next;
        Some((old_rsp, new_rsp, new_cr3, new_kstack_top))
    }
}

/// Repoints the syscall-entry stack (`syscall::PERCPU.kernel_stack`) and the
/// ring3-interrupt stack (TSS.`RSP0`) at `kstack_top` — call right before
/// `write_cr3()`/`context_switch()` at every switch, so whichever task ends
/// up "current" handles its own syscalls/interrupts on its own stack. See
/// `Task::_kstack`'s doc comment for the bug this fixes.
unsafe fn use_kstack(kstack_top: u64) {
    crate::syscall::set_kernel_stack(kstack_top);
    crate::gdt::set_tss_rsp0(kstack_top);
}

/// Array index (into `Scheduler::tasks`, *not* the ever-incrementing `id`
/// field) of whichever task is currently executing — the stable, bounded key
/// `process.rs` uses to look up per-process state (see its `ProcSlot` table),
/// since `id` grows without bound over a long session (every `spawn()` bumps
/// it) while this is bounded by however many tasks are alive *right now*.
pub fn current_task_index() -> usize {
    SCHEDULER.lock().current
}

/// Update the *currently running* task's own CR3 — called by `process.rs`
/// right after it changes CR3 itself (entering or leaving a process's
/// address space), so a future preemption saves/restores the right value
/// for this task instead of stale data left over from `Task::new()`.
pub fn set_current_cr3(cr3: u64) {
    let mut sched = SCHEDULER.lock();
    let cur = sched.current;
    sched.tasks[cur].cr3 = cr3;
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
    if let Some((old_rsp, new_rsp, new_cr3, new_kstack_top)) = switch {
        unsafe { use_kstack(new_kstack_top); write_cr3(new_cr3); context_switch(old_rsp, new_rsp); }
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
    if let Some((old_rsp, new_rsp, new_cr3, new_kstack_top)) = switch {
        unsafe { use_kstack(new_kstack_top); write_cr3(new_cr3); context_switch(old_rsp, new_rsp); }
    } else {
        // No other task to run — undo the Blocked state we tentatively set,
        // since nothing will ever call next() to promote us back to Ready.
        let mut sched = SCHEDULER.lock();
        let cur = sched.current;
        sched.tasks[cur].state = TaskState::Running;
    }
}

/// Block the calling task until interrupt vector `vector` fires (some other
/// context calls `wake_irq_waiters(vector)`), then yield to another task —
/// the event-driven counterpart to `sleep_ms()`'s time-driven block. Backs
/// `SYS_WAIT_IRQ`: a ring-3 process that wants to wait for its device's
/// interrupt instead of busy-polling makes that syscall, which calls this
/// from within the handler — blocking *this* (the syscall's own) task.
/// Falls straight through without blocking if no other task is `Ready`
/// (same reasoning as `sleep_ms()`: nothing would ever wake a lone task).
///
/// **A real, previously-latent `swapgs` bug this uncovered:** this function
/// is only ever called from inside `sys_wait_irq`'s syscall handler, i.e.
/// *after* `syscall_entry`'s entry-side `swapgs` already ran (GS.base is
/// currently `&PERCPU`) but *before* its matching exit-side `swapgs` (which
/// only runs once the syscall actually returns to ring 3 via `sysretq`).
/// `swapgs` is a hardware *toggle*, not an idempotent load — if this
/// function context-switches away here without undoing that toggle first,
/// whichever *other* task/syscall runs next inherits GS.base still pointing
/// at `&PERCPU` instead of its own ring-3 GS, and its own entry-side
/// `swapgs` then flips GS.base to *this* task's stale value instead —
/// corrupting `gs:[0]`/`gs:[8]` (`PERCPU.kernel_stack`/`user_rsp`) for
/// everyone downstream (this is what a second concurrent process's very
/// first syscall was crashing on — a `gs:[8]` write landing at physical
/// address 8, GS.base having decayed to near-zero). Fixed by swapping back
/// to the "resting" GS state before giving up the CPU, and swapping into
/// the "mid-syscall" state again once this task is actually resumed (right
/// where `context_switch()` returns, however much later that is) — from
/// every *other* task's perspective, GS.base is always exactly what it
/// should be for whatever they're doing.
///
/// **A second, closely-related bug the `swapgs` fix alone didn't cover:**
/// `gs:[8]` (`PERCPU.user_rsp`) is a *single* per-CPU scratch slot, not
/// per-task — `syscall_entry`'s asm stashes the calling task's user-mode
/// RSP there on entry and restores it from there on `sysretq`. If this task
/// blocks here mid-syscall and some *other* task's syscall entry runs
/// before this one resumes (routine, once several drivers are all calling
/// `SYS_WAIT_IRQ` every loop iteration), that other task's entry overwrites
/// `gs:[8]` with *its own* user RSP. When this task is eventually resumed
/// and its own `sysretq` fires, it restores whatever's *currently* sitting
/// in `gs:[8]` — not the RSP this task actually had — silently corrupting
/// its own ring-3 stack pointer (surfaced as a page fault at a seemingly
/// arbitrary address near the top of the user stack region, once enough
/// concurrent `SYS_WAIT_IRQ`-blocking drivers existed to make the race
/// likely). Fixed the same way as the `swapgs` toggle above: save this
/// task's own `gs:[8]` value to a local *before* yielding (nothing else can
/// have touched it yet — it's still this task's own, freshly written by its
/// own entry), then write it back immediately after resuming, before
/// anything else gets a chance to read it via this task's own eventual
/// `sysretq`.
pub fn block_on_irq(vector: u8) {
    let switch = {
        let mut sched = SCHEDULER.lock();
        let cur = sched.current;
        sched.tasks[cur].waiting_irq = Some(vector);
        sched.tasks[cur].wake_at = u64::MAX;
        sched.tasks[cur].state = TaskState::Blocked;
        sched.next()
    };
    if let Some((old_rsp, new_rsp, new_cr3, new_kstack_top)) = switch {
        unsafe {
            let saved_user_rsp = crate::syscall::get_user_rsp();
            core::arch::asm!("swapgs", options(nostack, nomem)); // → resting state before yielding the CPU
            use_kstack(new_kstack_top);
            write_cr3(new_cr3);
            context_switch(old_rsp, new_rsp);
            // Resumed here (possibly much later, on a different physical
            // moment entirely) — flip back to mid-syscall state so the
            // exit-side swapgs+sysretq waiting for us in syscall_entry
            // still sees what it expects, and restore our own user_rsp in
            // case some other task's syscall clobbered the shared slot
            // while we were away.
            core::arch::asm!("swapgs", options(nostack, nomem));
            crate::syscall::set_user_rsp(saved_user_rsp);
        }
    } else {
        let mut sched = SCHEDULER.lock();
        let cur = sched.current;
        sched.tasks[cur].state = TaskState::Running;
        sched.tasks[cur].waiting_irq = None;
    }
}

/// Promote every task blocked on `vector` (via `block_on_irq()`) back to
/// `Ready`. Meant to be called from that interrupt's own handler — currently
/// only the timer ISR does (see `tick()` below), since it's the only
/// interrupt source with a real handler at all today; any future real device
/// IRQ handler would call this the same way to wake its waiters.
pub fn wake_irq_waiters(vector: u8) {
    let mut sched = SCHEDULER.lock();
    for t in sched.tasks.iter_mut() {
        if t.state == TaskState::Blocked && t.waiting_irq == Some(vector) {
            t.waiting_irq = None;
            t.state = TaskState::Ready;
        }
    }
}

/// Wall-time tick counter — incremented every ~10 ms by the APIC timer ISR.
/// Safe to poll from any context without MMIO or TSC calibration.
pub static TICK_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Called from timer ISR. Drops the lock BEFORE switching stacks.
pub fn tick() {
    TICK_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    wake_irq_waiters(crate::apic::timer_vector());
    // Acquire lock, compute switch, then DROP lock before context_switch.
    let switch = SCHEDULER.lock().next();
    if let Some((old_rsp, new_rsp, new_cr3, new_kstack_top)) = switch {
        // Lock is released here — safe to switch stacks.
        unsafe { use_kstack(new_kstack_top); write_cr3(new_cr3); context_switch(old_rsp, new_rsp); }
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
