//! Temporary, tool-free side questions. Only numeric usage reaches the journal.

pub mod view;

use std::collections::VecDeque;

use crate::message::{Assistant, Message, OutputItem};
use crate::session::Snapshot;

pub const MAX_EXCHANGES: usize = 20;
pub const SIDE_RULES: &str = "Answer this quick side question using the supplied context. The main task continues independently. Do not perform the main task, call tools, change files, update todos, or ask structured questionnaires. Return only the side answer.";

#[derive(Clone)]
pub struct Exchange {
    pub question: String,
    pub answer: Assistant,
}

#[derive(Clone, Default)]
pub struct Thread {
    exchanges: VecDeque<Exchange>,
}

impl Thread {
    pub fn messages(&self, snapshot: &Snapshot, question: &str) -> Vec<Message> {
        let mut messages = snapshot.side_messages();
        for exchange in &self.exchanges {
            messages.push(Message::user(&exchange.question));
            let mut answer = exchange.answer.clone();
            // Old side reasoning was bound to an earlier main-context snapshot.
            answer
                .items
                .retain(|item| matches!(item, OutputItem::Message { .. }));
            messages.push(Message::Assistant { response: answer });
        }
        messages.push(Message::user(question));
        messages
    }

    pub fn push(&mut self, question: String, answer: Assistant) {
        self.exchanges.push_back(Exchange { question, answer });
        while self.exchanges.len() > MAX_EXCHANGES {
            self.exchanges.pop_front();
        }
    }

    pub fn latest(&self) -> Option<&Exchange> {
        self.exchanges.back()
    }
    pub fn len(&self) -> usize {
        self.exchanges.len()
    }
    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty()
    }
    pub fn clear(&mut self) {
        self.exchanges.clear();
    }
}
