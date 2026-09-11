//! 运行中转向 / 跟进队列（对标 Pi `steeringMode` / `followUpMode`）。

use rupi_core::{ContentBlock, Message, Role};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex;

/// 排队消息：文本 + 可选图片（RPC `images[]`）。
#[derive(Debug, Clone, Default)]
pub struct QueuedMessage {
    pub text: String,
    pub images: Vec<QueuedImage>,
}

#[derive(Debug, Clone)]
pub struct QueuedImage {
    pub media_type: String,
    pub data: String,
}

impl QueuedMessage {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn to_user_message(&self) -> Message {
        let mut blocks = Vec::new();
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }
        for img in &self.images {
            blocks.push(ContentBlock::Image {
                media_type: img.media_type.clone(),
                data: img.data.clone(),
            });
        }
        if blocks.is_empty() {
            blocks.push(ContentBlock::Text {
                text: String::new(),
            });
        }
        Message::from_blocks(Role::User, blocks)
    }
}

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
    steering: VecDeque<QueuedMessage>,
    follow_up: VecDeque<QueuedMessage>,
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
        self.steer_msg(QueuedMessage::from_text(message));
    }

    pub fn follow_up(&self, message: impl Into<String>) {
        self.follow_up_msg(QueuedMessage::from_text(message));
    }

    pub fn steer_msg(&self, message: QueuedMessage) {
        if message.text.trim().is_empty() && message.images.is_empty() {
            return;
        }
        self.inner.lock().unwrap().steering.push_back(message);
    }

    pub fn follow_up_msg(&self, message: QueuedMessage) {
        if message.text.trim().is_empty() && message.images.is_empty() {
            return;
        }
        self.inner.lock().unwrap().follow_up.push_back(message);
    }

    pub fn take_steering(&self) -> Vec<String> {
        self.take_steering_msgs()
            .into_iter()
            .map(|m| m.text)
            .collect()
    }

    pub fn take_follow_up(&self) -> Vec<String> {
        self.take_follow_up_msgs()
            .into_iter()
            .map(|m| m.text)
            .collect()
    }

    pub fn take_steering_msgs(&self) -> Vec<QueuedMessage> {
        let mut g = self.inner.lock().unwrap();
        let mode = g.steering_mode;
        take_by_mode(&mut g.steering, mode)
    }

    pub fn take_follow_up_msgs(&self) -> Vec<QueuedMessage> {
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
            .map(|m| m.text.clone())
            .collect()
    }

    pub fn peek_follow_up(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .follow_up
            .iter()
            .map(|m| m.text.clone())
            .collect()
    }

    /// 清空两队，返回已被丢掉的原文（对标 Pi `clear_queue`）。
    pub fn clear(&self) -> (Vec<String>, Vec<String>) {
        let mut g = self.inner.lock().unwrap();
        (
            g.steering.drain(..).map(|m| m.text).collect(),
            g.follow_up.drain(..).map(|m| m.text).collect(),
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

fn take_by_mode(q: &mut VecDeque<QueuedMessage>, mode: QueueMode) -> Vec<QueuedMessage> {
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

    #[test]
    fn queued_message_keeps_images() {
        let inbox = MessageInbox::new();
        inbox.steer_msg(QueuedMessage {
            text: "look".into(),
            images: vec![QueuedImage {
                media_type: "image/png".into(),
                data: "abc".into(),
            }],
        });
        let msgs = inbox.take_steering_msgs();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].images.len(), 1);
        assert!(msgs[0].to_user_message().has_images());
    }
}
