//! Ordered, bounded pending user inputs. Claim/commit separates editing from delivery.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::message::Content;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Steer,
    FollowUp,
}

pub struct Input {
    pub id: u64,
    pub kind: Kind,
    pub content: Vec<Content>,
    pub summary: String,
}
impl Input {
    fn bytes(&self) -> usize {
        self.content
            .iter()
            .map(|c| match c {
                Content::Text { text } => text.len(),
                Content::Image { data, .. } => data.len(),
            })
            .sum()
    }
}

#[derive(Clone)]
pub struct Pending {
    pub id: u64,
    pub kind: Kind,
    pub summary: String,
}

#[derive(Default)]
struct State {
    next_id: u64,
    pending: VecDeque<Input>,
    claimed_bytes: usize,
    claimed: bool,
}

#[derive(Clone, Default)]
pub struct Queue(Arc<Mutex<State>>);

impl Queue {
    pub fn push(&self, kind: Kind, content: Vec<Content>, summary: String) -> Result<u64, String> {
        let mut state = self.0.lock().expect("input queue mutex");
        let id = state
            .next_id
            .checked_add(1)
            .ok_or("Input queue ID limit reached")?;
        let input = Input {
            id,
            kind,
            content,
            summary,
        };
        if state.pending.len() + usize::from(state.claimed) >= 64
            || state.pending.iter().map(Input::bytes).sum::<usize>()
                + state.claimed_bytes
                + input.bytes()
                > 20 * 1024 * 1024
        {
            return Err("Pending input queue is full; edit queued messages or wait".into());
        }
        state.next_id = id;
        state.pending.push_back(input);
        Ok(id)
    }

    pub fn snapshot(&self) -> Vec<Pending> {
        self.0
            .lock()
            .expect("input queue mutex")
            .pending
            .iter()
            .map(|i| Pending {
                id: i.id,
                kind: i.kind,
                summary: i.summary.clone(),
            })
            .collect()
    }

    /// Build an editable draft atomically; on failure every pending input stays queued.
    pub fn edit<T>(
        &self,
        build: impl FnOnce(&VecDeque<Input>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self.0.lock().expect("input queue mutex");
        let result = build(&state.pending)?;
        state.pending.clear();
        Ok(result)
    }

    pub fn take_pending(&self) -> Vec<Input> {
        self.0
            .lock()
            .expect("input queue mutex")
            .pending
            .drain(..)
            .collect()
    }

    pub fn claim(&self, follow_up: bool) -> Option<Claim> {
        let mut state = self.0.lock().expect("input queue mutex");
        if state.claimed {
            return None;
        }
        let index = state
            .pending
            .iter()
            .position(|input| input.kind == Kind::Steer)
            .or_else(|| (follow_up && !state.pending.is_empty()).then_some(0))?;
        let input = state.pending.remove(index)?;
        state.claimed_bytes = input.bytes();
        state.claimed = true;
        Some(Claim {
            queue: self.clone(),
            input: Some(input),
        })
    }
}

pub struct Claim {
    queue: Queue,
    input: Option<Input>,
}
impl Claim {
    pub fn input(&self) -> &Input {
        self.input.as_ref().expect("uncommitted queue claim")
    }
    pub fn commit(mut self) {
        self.input.take();
        let mut state = self.queue.0.lock().expect("input queue mutex");
        state.claimed = false;
        state.claimed_bytes = 0;
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(input) = self.input.take() {
            let mut state = self.queue.0.lock().expect("input queue mutex");
            state.pending.push_front(input);
            state.claimed = false;
            state.claimed_bytes = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claims_hold_capacity_and_restore_failed_delivery_without_duplicates() {
        let queue = Queue::default();
        queue
            .push(Kind::FollowUp, vec![Content::text("later")], "later".into())
            .unwrap();
        queue
            .push(Kind::Steer, vec![Content::text("now")], "now".into())
            .unwrap();
        let claim = queue.claim(false).unwrap();
        assert_eq!(claim.input().summary, "now");
        assert!(queue.claim(true).is_none());
        assert_eq!(queue.take_pending().len(), 1);
        drop(claim);
        assert_eq!(queue.snapshot().len(), 1);
        queue.claim(false).unwrap().commit();
        assert!(queue.snapshot().is_empty());
    }
}
