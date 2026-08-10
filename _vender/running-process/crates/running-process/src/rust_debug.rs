use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

#[derive(Clone)]
struct RustDebugFrame {
    label: &'static str,
    file: &'static str,
    line: u32,
}

type ThreadStack = Arc<Mutex<Vec<RustDebugFrame>>>;

fn registry() -> &'static Mutex<HashMap<String, ThreadStack>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, ThreadStack>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn thread_key() -> String {
    let current = thread::current();
    match current.name() {
        Some(name) => format!("{name} ({:?})", current.id()),
        None => format!("{:?}", current.id()),
    }
}

fn thread_stack() -> ThreadStack {
    thread_local! {
        static STACK: RefCell<Option<ThreadStack>> = const { RefCell::new(None) };
    }

    STACK.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(existing) = slot.as_ref() {
            return Arc::clone(existing);
        }
        let handle = Arc::new(Mutex::new(Vec::new()));
        registry()
            .lock()
            .expect("rust debug registry mutex poisoned")
            .insert(thread_key(), Arc::clone(&handle));
        *slot = Some(Arc::clone(&handle));
        handle
    })
}

/// RAII guard that records one debug frame for the current thread.
///
/// Create values with [`Self::enter`]. Dropping the guard pops the frame from
/// the thread-local debug stack.
pub struct RustDebugScopeGuard {
    stack: ThreadStack,
}

impl RustDebugScopeGuard {
    /// Push a debug frame onto the current thread's stack.
    pub fn enter(label: &'static str, file: &'static str, line: u32) -> Self {
        let stack = thread_stack();
        stack
            .lock()
            .expect("rust debug stack mutex poisoned")
            .push(RustDebugFrame { label, file, line });
        Self { stack }
    }
}

impl Drop for RustDebugScopeGuard {
    fn drop(&mut self) {
        let _ = self
            .stack
            .lock()
            .expect("rust debug stack mutex poisoned")
            .pop();
    }
}

/// Render all non-empty thread debug stacks.
pub fn render_rust_debug_traces() -> String {
    let registry = registry()
        .lock()
        .expect("rust debug registry mutex poisoned");
    let mut items: Vec<_> = registry.iter().collect();
    items.sort_by(|left, right| left.0.cmp(right.0));

    let mut rendered = String::new();
    for (thread_name, stack) in items {
        let stack = stack.lock().expect("rust debug stack mutex poisoned");
        if stack.is_empty() {
            continue;
        }
        let _ = writeln!(&mut rendered, "thread: {thread_name}");
        for (index, frame) in stack.iter().enumerate() {
            let _ = writeln!(
                &mut rendered,
                "  {index}: {} ({}:{})",
                frame.label, frame.file, frame.line
            );
        }
    }
    rendered
}
