//! Cumulative caller-thread allocation regression, not a latency/peak-memory benchmark.
use rpi_harness::agent_harness::{AgentHarness, AgentLane};

struct AuditAllocator;
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[global_allocator]
static ALLOCATOR: AuditAllocator = AuditAllocator;
fn count(bytes: usize) {
    let _ = ENABLED.try_with(|enabled| {
        if enabled.get() {
            let _ = BYTES.try_with(|count| count.set(count.get() + bytes));
        }
    });
}
unsafe impl std::alloc::GlobalAlloc for AuditAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        count(layout.size());
        std::alloc::System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        count(layout.size());
        std::alloc::System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        count(size);
        std::alloc::System.realloc(ptr, layout, size)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout)
    }
}
async fn idle_append(h: &AgentHarness) -> usize {
    BYTES.with(|n| n.set(0));
    ENABLED.with(|e| e.set(true));
    let result = h
        .append_message(rpi_ai::UserMessage::new("tiny input", 0).into())
        .await;
    ENABLED.with(|e| e.set(false));
    result.unwrap();
    BYTES.with(|n| n.get())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_append_does_not_copy_cancelled_steering_history() {
    let h = AgentHarness::create(Default::default()).await.unwrap();
    let empty = idle_append(&h).await;
    let size = 64 * 1024;
    for _ in 0..100 {
        let q = h
            .steer(rpi_ai::UserMessage::new("x".repeat(size), 0).into())
            .await
            .unwrap();
        h.cancel_queued(&q.entry_id).await.unwrap();
    }
    assert!(h.queued_messages().await.unwrap().is_empty());
    let first = idle_append(&h).await;
    let second = idle_append(&h).await;
    eprintln!(
        "ALLOCATION_AUDIT empty={empty}; cancelled_steer_bytes={}; first={first}; second={second}",
        size * 100
    );
    // Allow small query bookkeeping; do not copy even one 64 KiB historical body.
    assert!(
        first < empty + size,
        "unexpected historical payload copies: {first}"
    );
    assert!(
        second < empty + size,
        "unexpected historical payload copies: {second}"
    );
}
