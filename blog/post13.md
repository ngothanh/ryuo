# Building a Disruptor in Rust: Ryuo — Part 13: Builder DSL — Type-Safe, Ergonomic Configuration

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 13 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 5 (sequence barriers), Part 6 (event handlers), Part 9 (multi-producer)

---

## Recap

Over the past 12 posts, we've built every major component of the Disruptor:

| Component | Post | Purpose |
|-----------|------|---------|
| Ring Buffer | 2A | Pre-allocated, cache-friendly event storage |
| Padding | 2B | False sharing prevention |
| Sequencer | 3a-c | Sequence claiming and coordination |
| Wait Strategies | 4 | Consumer blocking policies |
| Sequence Barrier | 5 | Dependency enforcement |
| Event Handlers | 6 | Zero-cost dispatch and batching |
| Publishing | 7 | Closure-based event writing |
| Batch Rewind | 8 | Retry without data loss |
| Multi-Producer | 9 | CAS-based multi-threaded publishing |
| EventPoller | 10 | Pull-based consumption |
| Dynamic Topologies | 11 | Runtime handler management |
| Panic Handling | 12 | Recovery and isolation |

But wiring these together is **painful**:

```rust
// Current: 15 lines of boilerplate for a simple 1P-1C setup
let ring_buffer = Arc::new(RingBuffer::new(1024, || OrderEvent::default()));
let wait_strategy = Arc::new(BusySpinWaitStrategy);
let sequencer = Arc::new(SingleProducerSequencer::new(1024, vec![]));
let barrier = SequenceBarrier::from_sequencer(&sequencer, vec![]);
let handler_seq = Arc::new(AtomicI64::new(-1));
let mut processor = BatchEventProcessor::new(
    Arc::clone(&ring_buffer),
    Arc::new(barrier),
    MyHandler::new(),
);
sequencer.add_gating_sequence(Arc::clone(&handler_seq));
let handle = std::thread::spawn(move || processor.run());
// ... and we haven't even started publishing yet
```

Every line is a chance to misconfigure: wrong buffer size, missing gating sequence, forgetting to start the processor. Java's `Disruptor` class solves this with a runtime-validated builder. We can do better — **compile-time validation** with zero runtime overhead.

---

## The Type-State Pattern

The key insight: encode configuration state in the **type system**. Methods that require certain configuration only exist on the type that has that configuration. Missing a required field? The method to proceed doesn't exist — the compiler rejects it.

```
Type-State Machine:

  RingBufferBuilder<T, NoBufferSize, NoProducerType>
       │
       ├── .buffer_size(1024)
       │         │
       │         ▼
       │   RingBufferBuilder<T, HasBufferSize, NoProducerType>
       │         │
       │         ├── .single_producer()
       │         │         │
       │         │         ▼
       │         │   RingBufferBuilder<T, HasBufferSize, HasProducerType>
       │         │         │
       │         │         └── .build(factory) ← ONLY available here
       │         │
       │         └── .multi_producer()
       │                   │
       │                   ▼
       │             RingBufferBuilder<T, HasBufferSize, HasProducerType>
       │                   │
       │                   └── .build(factory)
       │
       └── .single_producer()
                 │
                 ▼
           RingBufferBuilder<T, NoBufferSize, HasProducerType>
                 │
                 └── .buffer_size(1024)
                           │
                           ▼
                     RingBufferBuilder<T, HasBufferSize, HasProducerType>
                           │
                           └── .build(factory)
```

Two axes, four states — but `build()` only exists in one: `(HasBufferSize, HasProducerType)`. The other three states simply don't have a `build()` method.

### Step 1: Type-State Markers

```rust
use std::marker::PhantomData;

/// Marker: buffer size has not been set.
pub struct NoBufferSize;
/// Marker: buffer size has been set.
pub struct HasBufferSize;
/// Marker: producer type has not been chosen.
pub struct NoProducerType;
/// Marker: producer type has been chosen.
pub struct HasProducerType;

/// Which sequencer to create.
#[derive(Debug, Clone, Copy)]
pub enum ProducerType {
    /// Single-threaded producer — no CAS, no contention.
    /// Use when only one thread publishes events.
    Single,
    /// Multi-threaded producer — CAS-based claiming.
    /// Use when multiple threads publish events.
    Multi,
}
```

**Why zero-sized types?** `NoBufferSize`, `HasBufferSize`, etc. are empty structs. They exist only in the type system — the compiler erases them completely. No runtime representation, no memory cost, no branches.

### Step 2: The Builder

```rust
/// Type-safe Disruptor builder.
///
/// The two type parameters `BufferState` and `ProducerState` encode
/// which configuration steps have been completed. `build()` is only
/// available when both are in the `Has*` state.
///
/// ```rust
/// // ✅ Compiles: both required fields set
/// let d = RingBufferBuilder::<OrderEvent>::new()
///     .buffer_size(1024)
///     .single_producer()
///     .build(|| OrderEvent::default());
///

impl<T> RingBufferBuilder<T, NoBufferSize, NoProducerType> {
    pub fn new() -> Self {
        Self {
            buffer_size: None,
            wait_strategy: None,
            producer_type: None,
            _phantom: PhantomData,
        }
    }
}

// ── Axis 1: Buffer Size ───────────────────────────────────────────

/// Available only when buffer size has NOT been set.
/// After calling, the type transitions to `HasBufferSize`.
impl<T, P> RingBufferBuilder<T, NoBufferSize, P> {
    pub fn buffer_size(self, size: usize) -> RingBufferBuilder<T, HasBufferSize, P> {
        assert!(
            size.is_power_of_two(),
            "Buffer size must be a power of 2, got {}",
            size
        );
        RingBufferBuilder {
            buffer_size: Some(size),
            wait_strategy: self.wait_strategy,
            producer_type: self.producer_type,
            _phantom: PhantomData,
        }
    }
}

// ── Axis 2: Producer Type ─────────────────────────────────────────

/// Available only when producer type has NOT been chosen.
/// After calling, the type transitions to `HasProducerType`.
impl<T, B> RingBufferBuilder<T, B, NoProducerType> {
    /// Single-threaded producer. Uses `SingleProducerSequencer` —
    /// no CAS operations, lowest latency.
    pub fn single_producer(self) -> RingBufferBuilder<T, B, HasProducerType> {
        RingBufferBuilder {
            buffer_size: self.buffer_size,
            wait_strategy: self.wait_strategy,
            producer_type: Some(ProducerType::Single),
            _phantom: PhantomData,
        }
    }

    /// Multi-threaded producer. Uses `MultiProducerSequencer` —
    /// CAS-based claiming with availability buffer.
    pub fn multi_producer(self) -> RingBufferBuilder<T, B, HasProducerType> {
        RingBufferBuilder {
            buffer_size: self.buffer_size,
            wait_strategy: self.wait_strategy,
            producer_type: Some(ProducerType::Multi),
            _phantom: PhantomData,
        }
    }
}

// ── Optional: Wait Strategy (available in any state) ──────────────

impl<T, B, P> RingBufferBuilder<T, B, P> {
    /// Set a custom wait strategy. If not called, defaults to
    /// `BusySpinWaitStrategy` (lowest latency, highest CPU usage).
    pub fn wait_strategy(mut self, strategy: Box<dyn WaitStrategy>) -> Self {
        self.wait_strategy = Some(strategy);
        self
    }
}

// ── Build: ONLY available when both axes are satisfied ────────────

impl<T: Send + 'static> RingBufferBuilder<T, HasBufferSize, HasProducerType> {
    /// Construct the ring buffer and sequencer.
    ///
    /// `factory` creates default event instances to pre-fill the buffer.
    /// Called `buffer_size` times during construction.
    ///
    /// The `unwrap()` calls are safe: the type system guarantees
    /// that `buffer_size` and `producer_type` have been set.
    pub fn build<F>(self, factory: F) -> DisruptorBuilder<T>
    where
        F: Fn() -> T + Send,
    {
        let buffer_size = self.buffer_size.unwrap();
        let wait_strategy = self.wait_strategy
            .unwrap_or_else(|| Box::new(BusySpinWaitStrategy));
        let producer_type = self.producer_type.unwrap();

        DisruptorBuilder::new(buffer_size, producer_type, wait_strategy, factory)
    }
}
```

**Why `assert!` instead of `Result` for `is_power_of_two`?** This is a programming error, not a runtime condition. Passing a non-power-of-two buffer size is a bug in the calling code — it will never be correct at runtime if it's wrong at compile time. Panicking loudly is the right behavior. The type-state pattern already handles "forgot to set buffer size" at compile time; this catches "set buffer size to a wrong value" at runtime.

### Step 3: DisruptorBuilder — Topology Configuration

After `build()`, we have a `DisruptorBuilder` that holds the ring buffer and sequencer. Now we configure the handler topology:

```rust
/// Intermediate builder: ring buffer and sequencer are ready,
/// now configure handlers.
pub struct DisruptorBuilder<T> {
    ring_buffer: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    wait_strategy: Arc<dyn WaitStrategy>,
    handler_sequences: Vec<Arc<AtomicI64>>,
    processor_handles: Vec<JoinHandle<()>>,
}

impl<T: Send + 'static> DisruptorBuilder<T> {
    fn new<F>(
        buffer_size: usize,
        producer_type: ProducerType,
        wait_strategy: Box<dyn WaitStrategy>,
        factory: F,
    ) -> Self
    where
        F: Fn() -> T + Send,
    {
        let ring_buffer = Arc::new(RingBuffer::new(buffer_size, factory));
        let wait_strategy: Arc<dyn WaitStrategy> = Arc::from(wait_strategy);

        let sequencer: Arc<dyn Sequencer> = match producer_type {
            ProducerType::Single => {
                Arc::new(SingleProducerSequencer::new(buffer_size, vec![]))
            }
            ProducerType::Multi => {
                Arc::new(MultiProducerSequencer::new(buffer_size, vec![]))
            }
        };

        Self {
            ring_buffer,
            sequencer,
            wait_strategy,
            handler_sequences: Vec::new(),
            processor_handles: Vec::new(),
        }
    }

    /// Add a handler that depends on the sequencer's cursor
    /// (i.e., processes events directly from the producer).
    ///
    /// Returns `self` for chaining. The handler's thread starts
    /// immediately.
    pub fn handle_events_with<H>(mut self, handler: H) -> Self
    where
        H: EventConsumer<T> + 'static,
    {
        let handler_seq = Arc::new(AtomicI64::new(-1));
        let barrier = SequenceBarrier::from_sequencer(
            &self.sequencer,
            vec![], // depends on cursor only
        );

        let mut processor = BatchEventProcessor::new(
            Arc::clone(&self.ring_buffer),
            Arc::new(barrier),
            handler,
        );
        // Give the processor a reference to its own sequence
        // so it can update progress.
        let seq_clone = Arc::clone(&handler_seq);

        let handle = std::thread::spawn(move || {
            processor.run();
        });

        // Register as gating sequence so the producer
        // doesn't overwrite unread events.
        self.sequencer.add_gating_sequence(Arc::clone(&handler_seq));
        self.handler_sequences.push(handler_seq);
        self.processor_handles.push(handle);
        self
    }

    /// Add a handler that runs in parallel with the previous handler(s)
    /// (multicast topology). Same dependencies as `handle_events_with`.
    pub fn and<H>(self, handler: H) -> Self
    where
        H: EventConsumer<T> + 'static,
    {
        // `and` is identical to `handle_events_with` — both
        // depend on the cursor, not on each other.
        self.handle_events_with(handler)
    }

    /// Add a handler that depends on ALL previously added handlers
    /// (pipeline topology). This handler only sees events after
    /// all prior handlers have finished processing them.
    pub fn then<H>(mut self, handler: H) -> Self
    where
        H: EventConsumer<T> + 'static,
    {
        let handler_seq = Arc::new(AtomicI64::new(-1));

        // Depend on ALL previously registered handler sequences.
        let dependencies: Vec<Arc<AtomicI64>> = self.handler_sequences.clone();
        let barrier = SequenceBarrier::from_sequencer(
            &self.sequencer,
            dependencies,
        );

        let mut processor = BatchEventProcessor::new(
            Arc::clone(&self.ring_buffer),
            Arc::new(barrier),
            handler,
        );

        let handle = std::thread::spawn(move || {
            processor.run();
        });

        self.sequencer.add_gating_sequence(Arc::clone(&handler_seq));
        self.handler_sequences.push(handler_seq);
        self.processor_handles.push(handle);
        self
    }

    /// Finalize the builder and return a handle for publishing
    /// and shutdown.
    pub fn start(self) -> DisruptorHandle<T> {
        DisruptorHandle {
            ring_buffer: self.ring_buffer,
            sequencer: self.sequencer,
            processor_handles: self.processor_handles,
        }
    }
}
```

**Why `and()` is identical to `handle_events_with()`:** Both create handlers that depend only on the producer's cursor. They process events independently, in parallel. The naming difference is purely ergonomic — `and` reads better in a chain:

```rust
builder
    .handle_events_with(Validator::new())
    .and(Logger::new())        // parallel with Validator
    .then(Persister::new())    // waits for both
```

### Step 4: DisruptorHandle — Publishing and Shutdown

```rust
/// Owns the running Disruptor. Provides publishing and shutdown.
pub struct DisruptorHandle<T> {
    ring_buffer: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    processor_handles: Vec<JoinHandle<()>>,
}

impl<T> DisruptorHandle<T> {
    /// Publish a single event using the closure pattern from Part 7.
    pub fn publish_with<F>(&self, f: F)
    where
        F: FnOnce(&mut T, i64),
    {
        self.ring_buffer.publish_with(&*self.sequencer, f);
    }

    /// Try to publish without blocking. Returns `Err` if the
    /// ring buffer is full.
    pub fn try_publish_with<F>(&self, f: F) -> Result<(), InsufficientCapacity>
    where
        F: FnOnce(&mut T, i64),
    {
        self.ring_buffer.try_publish_with(&*self.sequencer, f)
    }

    /// Gracefully shut down: alert all barriers, then join all
    /// processor threads.
    pub fn shutdown(self) {
        // 1. Alert all sequence barriers to wake up waiting consumers
        self.sequencer.alert();

        // 2. Wait for all processor threads to finish
        for handle in self.processor_handles {
            handle.join().expect("Processor thread panicked during shutdown");
        }
    }
}
```

---

## Usage Examples

### Example 1: Simple 1P-1C (Compare with Boilerplate)

```rust
// Before: 15 lines of manual wiring
// After: 5 lines
let disruptor = RingBufferBuilder::<OrderEvent>::new()
    .buffer_size(1024)
    .single_producer()
    .build(|| OrderEvent::default())
    .handle_events_with(OrderValidator::new())
    .start();

// Publish
disruptor.publish_with(|event, seq| {
    event.order_id = 42;
    event.price = 100_50; // $100.50 in cents
});

// Shutdown
disruptor.shutdown();
```

### Example 2: Pipeline (Sequential Processing)

```rust
// Order: Validate → Enrich → Persist
// Each handler waits for the previous one.
let disruptor = RingBufferBuilder::<OrderEvent>::new()
    .buffer_size(4096)
    .single_producer()
    .wait_strategy(Box::new(BlockingWaitStrategy::new()))
    .build(|| OrderEvent::default())
    .handle_events_with(OrderValidator::new())
    .then(OrderEnricher::new())        // waits for Validator
    .then(OrderPersister::new())       // waits for Enricher
    .start();
```

### Example 3: Diamond Topology

```rust
// Validate and Log in parallel, then Persist after both.
//
//         ┌─ Validator ─┐
// Producer│              ├─ Persister
//         └─ Logger ─────┘
let disruptor = RingBufferBuilder::<OrderEvent>::new()
    .buffer_size(2048)
    .single_producer()
    .build(|| OrderEvent::default())
    .handle_events_with(OrderValidator::new())
    .and(OrderLogger::new())           // parallel with Validator
    .then(OrderPersister::new())       // waits for BOTH
    .start();
```

### Example 4: Compile-Time Error Prevention

```rust
// ❌ Missing buffer_size — build() doesn't exist on this type
let d = RingBufferBuilder::<OrderEvent>::new()
    .single_producer()
    .build(|| OrderEvent::default());
//  ^^^^^ error[E0599]: no method named `build` found for struct
//        `RingBufferBuilder<OrderEvent, NoBufferSize, HasProducerType>`

// ❌ Missing producer type — build() doesn't exist
let d = RingBufferBuilder::<OrderEvent>::new()
    .buffer_size(1024)
    .build(|| OrderEvent::default());
//  ^^^^^ error[E0599]: no method named `build` found for struct
//        `RingBufferBuilder<OrderEvent, HasBufferSize, NoProducerType>`

// ❌ Setting buffer_size twice — method doesn't exist
let d = RingBufferBuilder::<OrderEvent>::new()
    .buffer_size(1024)
    .buffer_size(2048);
//  ^^^^^^^^^^^ error[E0599]: no method named `buffer_size` found for struct
//              `RingBufferBuilder<OrderEvent, HasBufferSize, NoProducerType>`

// ❌ Non-power-of-2 — runtime panic (caught at build time in practice)
let d = RingBufferBuilder::<OrderEvent>::new()
    .buffer_size(1000)  // panic: "Buffer size must be a power of 2, got 1000"
    .single_producer()
    .build(|| OrderEvent::default());
```

**The error messages are actionable.** The compiler tells you exactly which type state you're in (`NoBufferSize`, `NoProducerType`) and which method is missing. No runtime `Result`, no "missing field" error at startup, no silent misconfiguration.

---

## Why Type-State Beats Runtime Validation

| Approach | When Errors Surface | Overhead | Error Quality |
|----------|-------------------|----------|---------------|
| No validation | Never (silent bug) | Zero | None — data corruption |
| Runtime `Result` | `build()` call site | `match`/`?` at call site | `Err(MissingBufferSize)` |
| Runtime `panic!` | `build()` call site | Zero (happy path) | Stack trace |
| **Type-state** | **Compile time** | **Zero** | **Type error with context** |

Java's `Disruptor` class uses runtime validation:

```java
// Java: compiles fine, fails at runtime
Disruptor<Event> d = new Disruptor<>(Event::new, /* missing buffer size */);
// throws IllegalArgumentException at runtime
```

Rust's type-state makes this a compile-time error. The `build()` method literally does not exist until all required configuration is provided.

**The only runtime check that remains:** `assert!(size.is_power_of_two())`. This is a *value* constraint, not a *presence* constraint. Type-state handles presence ("did you call `buffer_size`?"); assertions handle values ("did you pass a valid buffer size?").

---

## Optional: Macro DSL

For common configurations, a macro reduces even the builder's boilerplate:

```rust
/// Declarative Disruptor configuration.
///
/// Expands to a `RingBufferBuilder` chain with compile-time validation.
#[macro_export]
macro_rules! disruptor {
    (
        buffer_size: $size:expr,
        producer: $producer:ident,
        $(wait_strategy: $strategy:expr,)?
        factory: $factory:expr,
        handlers: [ $($handler:expr),* $(,)? ]
    ) => {{
        let mut builder = $crate::RingBufferBuilder::new()
            .buffer_size($size)
            .$producer();

        $(
            builder = builder.wait_strategy(Box::new($strategy));
        )?

        let mut disruptor_builder = builder.build($factory);

        $(
            disruptor_builder = disruptor_builder.handle_events_with($handler);
        )*

        disruptor_builder.start()
    }};
}

// Usage:
let disruptor = disruptor! {
    buffer_size: 1024,
    producer: single_producer,
    wait_strategy: BusySpinWaitStrategy,
    factory: || OrderEvent::default(),
    handlers: [
        OrderValidator::new(),
        OrderLogger::new(),
        OrderPersister::new(),
    ]
};
```

**When to use the macro vs the builder:**
- **Macro:** All handlers are independent (multicast). No `then()` — all handlers use `handle_events_with`.
- **Builder:** Pipelines, diamonds, or any topology with dependencies. Use `then()` for sequential, `and()` for parallel.

---

## Advanced: TopologyBuilder for Complex Graphs

For topologies beyond simple pipelines and diamonds, the `TopologyBuilder` provides graph-based configuration:

```rust
/// Graph-based topology builder for complex handler dependencies.
///
/// Handlers are identified by index. Dependencies are specified
/// as indices of handlers that must complete before this handler
/// can process an event.
pub struct TopologyBuilder<T: Send + 'static> {
    ring_buffer: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    wait_strategy: Arc<dyn WaitStrategy>,
    nodes: Vec<HandlerNode<T>>,
}

struct HandlerNode<T> {
    handler: Box<dyn EventConsumer<T>>,
    dependencies: Vec<usize>,
}

impl<T: Send + 'static> TopologyBuilder<T> {
    /// Add a handler with explicit dependencies.
    ///
    /// Returns the handler's index for use in other handlers'
    /// dependency lists.
    ///
    /// ```rust
    /// let mut topo = TopologyBuilder::new(ring_buffer, sequencer, wait_strategy);
    /// let validator = topo.add_handler(Validator::new(), &[]);     // no deps
    /// let enricher  = topo.add_handler(Enricher::new(), &[]);      // no deps
    /// let persister = topo.add_handler(Persister::new(), &[validator, enricher]);
    /// topo.start();
    /// ```
    pub fn add_handler<H>(
        &mut self,
        handler: H,
        depends_on: &[usize],
    ) -> usize
    where
        H: EventConsumer<T> + 'static,
    {
        // Validate: all dependencies must be valid indices
        for &dep in depends_on {
            assert!(
                dep < self.nodes.len(),
                "Dependency index {} out of range (only {} handlers registered)",
                dep,
                self.nodes.len()
            );
        }

        let index = self.nodes.len();
        self.nodes.push(HandlerNode {
            handler: Box::new(handler),
            dependencies: depends_on.to_vec(),
        });
        index
    }

    /// Validate and start the topology.
    ///
    /// Checks for cycles using DFS, then starts handler threads
    /// in dependency order.
    pub fn start(self) -> Vec<JoinHandle<()>> {
        self.validate_no_cycles();

        let mut sequences: Vec<Arc<AtomicI64>> = Vec::new();
        let mut handles = Vec::new();

        for node in self.nodes {
            let handler_seq = Arc::new(AtomicI64::new(-1));

            // Build dependency list from handler indices
            let dep_sequences: Vec<Arc<AtomicI64>> = node.dependencies
                .iter()
                .map(|&i| Arc::clone(&sequences[i]))
                .collect();

            let barrier = SequenceBarrier::from_sequencer(
                &self.sequencer,
                dep_sequences,
            );

            let mut processor = BatchEventProcessor::new(
                Arc::clone(&self.ring_buffer),
                Arc::new(barrier),
                node.handler,
            );

            self.sequencer.add_gating_sequence(Arc::clone(&handler_seq));
            sequences.push(handler_seq);

            let handle = std::thread::spawn(move || {
                processor.run();
            });
            handles.push(handle);
        }

        handles
    }

    /// DFS-based cycle detection.
    fn validate_no_cycles(&self) {
        #[derive(Clone, Copy, PartialEq)]
        enum Color { White, Gray, Black }

        let n = self.nodes.len();
        let mut colors = vec![Color::White; n];

        fn dfs<T>(
            nodes: &[HandlerNode<T>],
            colors: &mut [Color],
            u: usize,
        ) {
            colors[u] = Color::Gray;
            for &dep in &nodes[u].dependencies {
                match colors[dep] {
                    Color::Gray => panic!(
                        "Cycle detected in handler topology: \
                         handler {} depends on handler {}, which depends back on {}",
                        u, dep, u
                    ),
                    Color::White => dfs(nodes, colors, dep),
                    Color::Black => {} // already visited
                }
            }
            colors[u] = Color::Black;
        }

        for i in 0..n {
            if colors[i] == Color::White {
                dfs(&self.nodes, &mut colors, i);
            }
        }
    }
}
```

**Why DFS for cycle detection?** The handler dependency graph is a DAG (directed acyclic graph). Cycles would deadlock the system: handler A waits for handler B, which waits for handler A. The DFS-based three-color algorithm detects cycles in O(V + E) time during `start()`.

---

## Performance Characteristics

| Operation | Cost | Notes |
|-----------|------|-------|
| `RingBufferBuilder::new()` | 0 ns | Compiles to nothing (PhantomData) |
| `.buffer_size()` | 0 ns | State transition + assertion |
| `.single_producer()` | 0 ns | State transition |
| `.build(factory)` | O(N) | Pre-allocates N events |
| `.handle_events_with()` | ~10μs | Spawns a thread |
| `.start()` | 0 ns | Moves ownership |
| `disruptor!` macro | 0 ns | Expands to builder chain |

**Total setup cost:** O(buffer_size) for pre-allocation + O(num_handlers × 10μs) for thread spawning. Both are startup-only costs. The builder itself has **zero runtime overhead** — all type-state checks are erased by the compiler.

---

## Testing

### Test 1: Builder Produces Working Disruptor

```rust
#[test]
fn builder_produces_working_disruptor() {
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingHandler {
        count: Arc<AtomicU64>,
    }

    impl EventConsumer<u64> for CountingHandler {
        fn consume(&mut self, _event: &u64, _seq: i64, _eob: bool) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let count = Arc::new(AtomicU64::new(0));
    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(64)
        .single_producer()
        .build(|| 0u64)
        .handle_events_with(CountingHandler { count: Arc::clone(&count) })
        .start();

    for i in 0..100 {
        disruptor.publish_with(|event, _seq| {
            *event = i;
        });
    }

    disruptor.shutdown();
    assert_eq!(count.load(Ordering::Relaxed), 100);
}
```

### Test 2: Pipeline Ordering

```rust
#[test]
fn pipeline_preserves_ordering() {
    use std::sync::Mutex;

    struct RecordingHandler {
        name: String,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl EventConsumer<u64> for RecordingHandler {
        fn consume(&mut self, event: &u64, seq: i64, _eob: bool) {
            self.log.lock().unwrap().push(
                format!("{}:{}", self.name, seq)
            );
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let log = Arc::new(Mutex::new(Vec::new()));

    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(64)
        .single_producer()
        .build(|| 0u64)
        .handle_events_with(RecordingHandler {
            name: "A".to_string(),
            log: Arc::clone(&log),
        })
        .then(RecordingHandler {
            name: "B".to_string(),
            log: Arc::clone(&log),
        })
        .start();

    for i in 0..10 {
        disruptor.publish_with(|event, _| { *event = i; });
    }

    disruptor.shutdown();

    let entries = log.lock().unwrap();
    // For each sequence, A must appear before B
    for seq in 0..10 {
        let a_pos = entries.iter().position(|e| *e == format!("A:{}", seq));
        let b_pos = entries.iter().position(|e| *e == format!("B:{}", seq));
        assert!(
            a_pos.unwrap() < b_pos.unwrap(),
            "A must process seq {} before B",
            seq
        );
    }
}
```

### Test 3: Non-Power-of-Two Panics

```rust
#[test]
#[should_panic(expected = "Buffer size must be a power of 2")]
fn non_power_of_two_panics() {
    let _ = RingBufferBuilder::<u64>::new()
        .buffer_size(1000)
        .single_producer()
        .build(|| 0u64);
}
```

### Test 4: TopologyBuilder Rejects Invalid Dependencies

```rust
#[test]
#[should_panic(expected = "Dependency index 99 out of range")]
fn topology_builder_rejects_invalid_deps() {
    let ring_buffer = Arc::new(RingBuffer::new(64, || 0u64));
    let sequencer = Arc::new(SingleProducerSequencer::new(64, vec![]));
    let wait = Arc::new(BusySpinWaitStrategy);

    struct NoopHandler;
    impl EventConsumer<u64> for NoopHandler {
        fn consume(&mut self, _: &u64, _: i64, _: bool) {}
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let mut topo = TopologyBuilder::new(ring_buffer, sequencer, wait);
    let _a = topo.add_handler(NoopHandler, &[]);  // index 0

    // Dependency index 99 doesn't exist — should panic
    topo.add_handler(NoopHandler, &[99]);
}
```

> **Note:** The index-based API inherently prevents cycles — you can only depend on handlers added *before* you. The DFS validation in `validate_no_cycles` is a defense-in-depth measure for future API extensions.

### Test 5: Multi-Producer Builder

```rust
#[test]
fn multi_producer_builder_works() {
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingHandler {
        count: Arc<AtomicU64>,
    }

    impl EventConsumer<u64> for CountingHandler {
        fn consume(&mut self, _: &u64, _: i64, _: bool) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let count = Arc::new(AtomicU64::new(0));
    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(256)
        .multi_producer()
        .build(|| 0u64)
        .handle_events_with(CountingHandler { count: Arc::clone(&count) })
        .start();

    // Publish from multiple threads
    let handles: Vec<_> = (0..4).map(|_| {
        let d = disruptor.clone();
        std::thread::spawn(move || {
            for i in 0..25 {
                d.publish_with(|event, _| { *event = i; });
            }
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }

    disruptor.shutdown();
    assert_eq!(count.load(Ordering::Relaxed), 100);
}
```

---

## Key Takeaways

1. **Type-state eliminates misconfiguration at compile time.** Missing `buffer_size`? Missing `producer_type`? The `build()` method doesn't exist. No runtime errors, no `Result` types, no silent bugs.

2. **Two axes, one build gate.** `BufferState × ProducerState` creates four type states. Only `(HasBufferSize, HasProducerType)` has `build()`. The other three states are dead ends — the compiler enforces this.

3. **Zero runtime overhead.** `PhantomData` and zero-sized marker types are erased by the compiler. The builder collapses to direct construction calls after monomorphization.

4. **Fluent API reads like a specification.** `.handle_events_with(A).and(B).then(C)` clearly expresses "A and B in parallel, then C." Compare with 15 lines of manual wiring.

5. **The macro is sugar, not a replacement.** Use `disruptor!` for simple multicast topologies. Use the builder for pipelines and diamonds. Use `TopologyBuilder` for complex graphs.

6. **Cycle detection is O(V + E) at startup.** The DFS-based check runs once during `start()`. Runtime cost is negligible compared to thread spawning.

---

## Next Up: Production Patterns

In **Part 14**, we'll cover production-grade concerns:

- **Monitoring** — Queue depth, throughput, latency metrics
- **Backpressure** — Block, drop, sample strategies
- **Graceful shutdown** — Drain the ring buffer before exit
- **Anti-patterns** — Common mistakes and how to avoid them

---

## References

### Rust Documentation

- [PhantomData](https://doc.rust-lang.org/std/marker/struct.PhantomData.html) — Zero-sized type for phantom type parameters
- [std::thread::spawn](https://doc.rust-lang.org/std/thread/fn.spawn.html) — Thread creation
- [macro_rules!](https://doc.rust-lang.org/reference/macros-by-example.html) — Declarative macros

### Type-State Pattern

- [Type-State Pattern in Rust](https://cliffle.com/blog/rust-typestate/) — Cliff Biffle's canonical introduction
- [Builder Pattern with Type State](https://www.greyblake.com/blog/builder-with-typestate-in-rust/) — Greyblake's builder-specific guide

### Java Disruptor Reference

- [Disruptor.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/dsl/Disruptor.java) — Java's runtime-validated builder
- [EventHandlerGroup.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/dsl/EventHandlerGroup.java) — Java's `then()`/`and()` equivalent

---

**Next:** [Part 14 — Production Patterns: Monitoring, Backpressure, and Graceful Shutdown →](post14.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*