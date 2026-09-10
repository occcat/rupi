use std::collections::VecDeque;
use std::sync::Mutex;

use rupi_ai::Message;
use serde::{Deserialize, Serialize};

/// Controls how many queued user messages are injected at a drain point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    All,
    OneAtATime,
}

#[derive(Debug, Default)]
pub struct MessageQueue {
    inner: Mutex<VecDeque<Message>>,
    pub mode: QueueMode,
}

impl MessageQueue {
    pub fn new(mode: QueueMode) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            mode,
        }
    }

    pub fn push(&self, message: Message) {
        self.inner.lock().unwrap().push_back(message);
    }

    pub fn drain(&self) -> Vec<Message> {
        let mut q = self.inner.lock().unwrap();
        match self.mode {
            QueueMode::All => q.drain(..).collect(),
            QueueMode::OneAtATime => q.pop_front().into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}
