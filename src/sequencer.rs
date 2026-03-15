use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

pub trait Sequencer: Send {
    /// Claim `count` slots. Blocks until available.
    fn claim(&self, count: usize) -> SequenceClaim;

    /// Try to claim `count` slots. Returns immediately.
    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity>;

    /// Get current cursor position
    fn cursor(&self) -> i64;
}

#[derive(Debug, Clone, Copy)]
pub struct InsufficientCapacity;

pub struct SequenceClaim {
    start: i64,
    end: i64,
    publish_strategy: PublishStrategy,
}

enum PublishStrategy {
    Cursor {
        cursor: Arc<AtomicI64>,
    },
    AvailableBuffer {
        available_buffer: Arc<[AtomicI32]>,
        index_mask: usize,
        buffer_size: usize,
    },
}

impl SequenceClaim {
    pub fn start(&self) -> i64 {
        self.start
    }

    pub fn end(&self) -> i64 {
        self.end
    }

    pub fn iter(&self) -> impl Iterator<Item = i64> {
        self.start..=self.end
    }
}

impl Drop for SequenceClaim {
    fn drop(&mut self) {
        match (&self.publish_strategy) {
            PublishStrategy::Cursor { cursor } => cursor.store(self.end, Ordering::Release),
            PublishStrategy::AvailableBuffer {
                available_buffer,
                index_mask,
                buffer_size,
            } => {
                for seq in self.start..=self.end {
                    let i = seq as usize & index_mask;
                    let lap = (seq / *buffer_size as i64) as i32;
                    available_buffer[i].store(lap, Ordering::Release);
                }
            }
        }
    }
}

pub struct SingleProducerSequencer {
    cursor: Arc<AtomicI64>,
    next_sequence: Cell<i64>,
    buffer_size: usize,
    gating_sequences: Vec<Arc<AtomicI64>>,
    cached_gating_sequences: AtomicI64,
}

impl SingleProducerSequencer {
    pub fn new(buffer_size: usize, gating_sequences: Vec<Arc<AtomicI64>>) -> Self {
        Self {
            cursor: Arc::new(AtomicI64::new(-1)),
            next_sequence: Cell::new(0),
            buffer_size,
            gating_sequences,
            cached_gating_sequences: AtomicI64::new(-1),
        }
    }

    pub fn cursor_arc(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.cursor)
    }

    fn get_minimum_sequence(&self) -> i64 {
        let mut min = i64::MAX;
        for seq in &self.gating_sequences {
            let value = seq.load(Ordering::Acquire);
            min = min.min(value);
        }
        self.cached_gating_sequences.store(min, Ordering::Relaxed);
        min
    }
}

impl Sequencer for SingleProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        todo!()
    }

    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        todo!()
    }

    fn cursor(&self) -> i64 {
        todo!()
    }
}
