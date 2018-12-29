use crate::immix::Heap;
use crate::mailbox::Mailbox;
use crate::module::Module;
use crate::pool::Job;
pub use crate::process_table::PID;
use crate::value::Value;
use crate::vm::RcState;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::panic::RefUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::exception::Exception;

/// Heavily inspired by inko

pub type RcProcess = Arc<Process>;

// TODO: max registers should be a MAX_REG constant for (x and freg), OTP uses 1024
// regs should be growable and shrink on live
// also, only store "live" regs in the execution context and swap them into VM/scheduler
// ---> sched should have it's own ExecutionContext
// also this way, regs could be a &mut [] slice with no clone?

pub struct ExecutionContext {
    /// X registers.
    pub x: [Value; 16],
    /// Floating point registers.
    pub f: [f64; 16],
    /// Stack (accessible through Y registers).
    pub stack: Vec<Value>,
    pub heap: Heap,
    /// Number of catches on stack.
    pub catches: usize,
    /// Program pointer, points to the current instruction.
    pub ip: usize, // TODO: ip/cp need to store (offset as usize, *const Module)
    /// Continuation pointer
    pub cp: Option<usize>,
    pub live: usize,
    /// pointer to the current module
    pub module: *const Module,
    /// binary construction state
    pub bs: *mut String,
    /// 
    pub exc: Option<Exception>
}

pub struct InstrPtr {
    /// Module containing the instruction set.
    pub module: *const Module,
    /// Offset to the current instruction.
    pub ip: usize,
}

impl ExecutionContext {
    pub fn new(module: *const Module) -> ExecutionContext {
        unsafe {
            let mut ctx = ExecutionContext {
                x: std::mem::uninitialized(), //[Value::Nil(); 16],
                f: [0.0f64; 16],
                stack: Vec::new(),
                heap: Heap::new(),
                catches: 0,
                ip: 0,
                cp: None,
                live: 0,

                // register: Register::new(block.code.registers as usize),
                // binding: Binding::with_rc(block.locals(), block.receiver),
                module,
                // line: block.code.line,

                // TODO: not great
                bs: std::mem::uninitialized(),
            };
            for (_i, el) in ctx.x.iter_mut().enumerate() {
                // Overwrite `element` without running the destructor of the old value.
                // Since Value does not implement Copy, it is moved.
                std::ptr::write(el, Value::Nil());
            }
            ctx
        }
    }
}

pub struct LocalData {
    // allocator, panic handler
    context: Box<ExecutionContext>,

    pub mailbox: Mailbox,

    /// The ID of the thread this process is pinned to.
    pub thread_id: Option<u8>,

    /// A [process dictionary](https://www.erlang.org/course/advanced#dict)
    pub dictionary: HashMap<Value, Value>,
}

pub struct Process {
    /// Data stored in a process that should only be modified by a single thread
    /// at once.
    pub local_data: UnsafeCell<LocalData>,

    /// The process identifier of this process.
    pub pid: PID,

    /// If the process is waiting for a message.
    pub waiting_for_message: AtomicBool,
}

unsafe impl Sync for LocalData {}
unsafe impl Send for LocalData {}
unsafe impl Sync for Process {}
impl RefUnwindSafe for Process {}

impl Process {
    pub fn with_rc(
        pid: PID,
        context: ExecutionContext,
        // global_allocator: RcGlobalAllocator,
        // config: &Config,
    ) -> RcProcess {
        let local_data = LocalData {
            // allocator: LocalAllocator::new(global_allocator.clone(), config),
            context: Box::new(context),
            mailbox: Mailbox::new(),
            thread_id: None,
            dictionary: HashMap::new(),
        };

        Arc::new(Process {
            pid,
            local_data: UnsafeCell::new(local_data),
            waiting_for_message: AtomicBool::new(false),
        })
    }

    pub fn from_block(
        pid: PID,
        module: *const Module,
        // global_allocator: RcGlobalAllocator,
        // config: &Config,
    ) -> RcProcess {
        let context = ExecutionContext::new(module);

        Process::with_rc(pid, context /*global_allocator, config*/)
    }

    #[allow(clippy::mut_from_ref)]
    pub fn context_mut(&self) -> &mut ExecutionContext {
        &mut *self.local_data_mut().context
    }

    #[allow(clippy::mut_from_ref)]
    pub fn local_data_mut(&self) -> &mut LocalData {
        unsafe { &mut *self.local_data.get() }
    }

    pub fn local_data(&self) -> &LocalData {
        unsafe { &*self.local_data.get() }
    }

    pub fn is_main(&self) -> bool {
        self.pid == 0
    }

    pub fn send_message(&self, sender: &RcProcess, message: &Value) {
        if sender.pid == self.pid {
            self.local_data_mut().mailbox.send_internal(message);
        } else {
            self.local_data_mut().mailbox.send_external(message);
        }
    }

    pub fn set_waiting_for_message(&self, value: bool) {
        self.waiting_for_message.store(value, Ordering::Relaxed);
    }

    pub fn is_waiting_for_message(&self) -> bool {
        self.waiting_for_message.load(Ordering::Relaxed)
    }
}

pub fn allocate(state: &RcState, module: *const Module) -> Result<RcProcess, String> {
    let mut process_table = state.process_table.lock().unwrap();

    let pid = process_table
        .reserve()
        .ok_or_else(|| "No PID could be reserved".to_string())?;

    let process = Process::from_block(
        pid, module, /*, state.global_allocator.clone(), &state.config*/
    );

    process_table.map(pid, process.clone());

    Ok(process)
}

pub fn spawn(
    state: &RcState,
    module: *const Module,
    func: usize,
    args: Value,
) -> Result<Value, String> {
    println!("Spawning..");
    // let block_obj = block_ptr.block_value()?;
    let new_proc = allocate(state, module)?;
    let new_pid = new_proc.pid;
    // let pid_ptr = new_proc.allocate_usize(new_pid, state.integer_prototype);
    let pid_ptr = Value::Pid(new_pid);

    // TODO: func to ip offset
    let func = unsafe {
        (*module)
            .funs
            .get(&(func, 1)) // TODO: figure out arity from arglist?
            .expect("process::spawn could not locate func")
    };
    let context = new_proc.context_mut();
    context.ip = *func;

    // arglist to process registers,
    // TODO: it also needs to deep clone all the vals (for example lists etc)
    unsafe {
        let mut i = 0;
        let mut cons = &args;
        while let Value::List(ptr) = *cons {
            context.x[i] = (*ptr).head.clone();
            i += 1;
            cons = &(*ptr).tail;
        }
        // lastly, the tail
        context.x[i] = (*cons).clone();
    }

    state.process_pool.schedule(Job::normal(new_proc));

    Ok(pid_ptr)
}

pub fn send_message<'a>(
    state: &RcState,
    process: &RcProcess,
    // TODO: use pointers for these
    pid: &Value,
    msg: &'a Value,
) -> Result<&'a Value, Exception> {
    let pid = pid.to_usize();

    if let Some(receiver) = state.process_table.lock().unwrap().get(pid) {
        receiver.send_message(process, msg);

        if receiver.is_waiting_for_message() {
            // wake up
            receiver.set_waiting_for_message(false);

            state.process_pool.schedule(Job::normal(receiver));
        }
    }

    Ok(msg)
}
