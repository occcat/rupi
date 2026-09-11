//! 运行中转向 / 跟进队列（对标 Pi `steeringMode` / `followUpMode`）。

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex;

/// 队列一次取出一条还是全部。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QueueMode {
    /// 一次取出队列里全部消息。
    #[serde(rename = "all")]
    All,
    /// 每次只取队首一条（Pi 默认）。
    #[default]
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

impl QueueMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "all" => Some(Self::All),
            "one-at-a-time" | "one_at_a_time" => Some(Self::OneAtATime),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OneAtATime => "one-at-a-time",
        }
    }
}

#[derive(Debug)]
struct Inner {
    steering: VecDeque<String>,
    follow_up: VecDeque<String>,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

/// 跨任务共享的转向/跟进信箱。`AgentLoop::run` 在工具间隙取转向；
/// `AgentSession` 在整轮结束后取跟进。
#[derive(Debug)]
pub struct MessageInbox {
    inner: Mutex<Inner>,
}

impl Default for MessageInbox {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageInbox {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                steering: VecDeque::new(),
                follow_up: VecDeque::new(),
                steering_mode: QueueMode::OneAtATime,
                follow_up_mode: QueueMode::OneAtATime,
            }),
        }
    }

    pub fn steer(&self, message: impl Into<String>) {
        let m = message.into();
        if m.trim().is_empty() {
            return;
        }
        self.inner.lock().unwrap().steering.push_back(m);
    }

    pub fn follow_up(&self, message: impl Into<String>) {
        let m = message.into();
        if m.trim().is_empty() {
            return;
        }
        self.inner.lock().unwrap().follow_up.push_back(m);
    }

    pub fn take_steering(&self) -> Vec<String> {
        let mut g = self.inner.lock().unwrap();
        let mode = g.steering_mode;
        take_by_mode(&mut g.steering, mode)
    }

    pub fn take_follow_up(&self) -> Vec<String> {
        let mut g = self.inner.lock().unwrap();
        let mode = g.follow_up_mode;
        take_by_mode(&mut g.follow_up, mode)
    }

    pub fn peek_steering(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .steering
            .iter()
            .cloned()
            .collect()
    }

    pub fn peek_follow_up(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .follow_up
            .iter()
            .cloned()
            .collect()
    }

    /// 清空两队，返回已被丢掉的原文（对标 Pi `clear_queue`）。
    pub fn clear(&self) -> (Vec<String>, Vec<String>) {
        let mut g = self.inner.lock().unwrap();
        (
            g.steering.drain(..).collect(),
            g.follow_up.drain(..).collect(),
        )
    }

    pub fn pending_count(&self) -> usize {
        let g = self.inner.lock().unwrap();
        g.steering.len() + g.follow_up.len()
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.inner.lock().unwrap().steering_mode
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.inner.lock().unwrap().follow_up_mode
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.lock().unwrap().steering_mode = mode;
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.lock().unwrap().follow_up_mode = mode;
    }
}

fn take_by_mode(q: &mut VecDeque<String>, mode: QueueMode) -> Vec<String> {
    match mode {
        QueueMode::All => q.drain(..).collect(),
        QueueMode::OneAtATime => q.pop_front().into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_at_a_time_then_all() {
        let inbox = MessageInbox::new();
        inbox.steer("a");
        inbox.steer("b");
        inbox.follow_up("c");
        inbox.follow_up("d");
        assert_eq!(inbox.take_steering(), vec!["a".to_string()]);
        assert_eq!(inbox.take_steering(), vec!["b".to_string()]);
        assert!(inbox.take_steering().is_empty());
        inbox.set_follow_up_mode(QueueMode::All);
        assert_eq!(
            inbox.take_follow_up(),
            vec!["c".to_string(), "d".to_string()]
        );
        let (s, f) = inbox.clear();
        assert!(s.is_empty() && f.is_empty());
    }

    #[test]
    fn clear_returns_queued_text() {
        let inbox = MessageInbox::new();
        inbox.steer("turn");
        inbox.follow_up("later");
        let (s, f) = inbox.clear();
        assert_eq!(s, vec!["turn".to_string()]);
        assert_eq!(f, vec!["later".to_string()]);
        assert_eq!(inbox.pending_count(), 0);
    }

    #[test]
    fn parse_mode() {
        assert_eq!(QueueMode::parse("all"), Some(QueueMode::All));
        assert_eq!(
            QueueMode::parse("one-at-a-time"),
            Some(QueueMode::OneAtATime)
        );
        assert_eq!(QueueMode::parse("nope"), None);
    }
}
