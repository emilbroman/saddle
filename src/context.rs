//! Request-only context budgeting; the durable conversation is never shortened.

use genai::chat::{ChatMessage, ChatRequest, ChatRole};
use std::collections::HashSet;

pub struct ContextBudget {
    window: usize,
    start: usize,
    measured: Option<(usize, usize)>,
    request_estimate: usize,
}

impl ContextBudget {
    pub fn new(window: usize) -> Self {
        Self {
            window,
            start: 0,
            measured: None,
            request_estimate: 0,
        }
    }

    pub fn reset(&mut self) {
        self.start = 0;
        self.measured = None;
        self.request_estimate = 0;
    }

    pub fn observe(&mut self, tokens: usize) {
        self.measured = Some((tokens, self.request_estimate));
    }

    fn estimated(&self, estimate: usize) -> usize {
        match self.measured {
            Some((tokens, previous)) if estimate >= previous => {
                tokens.saturating_add(estimate - previous)
            }
            Some((tokens, previous)) => tokens.saturating_sub(previous - estimate),
            None => estimate,
        }
    }

    /// Remove old turns and complete tool roundtrips, retaining the latest user
    /// input and newest group even when those alone exceed the budget.
    pub fn prepare(&mut self, request: &mut ChatRequest, force: bool) -> bool {
        let old_start = self.start;
        let all = std::mem::take(&mut request.messages);
        let latest_user = all
            .iter()
            .rposition(|message| matches!(message.role, ChatRole::User));
        let mut pending = HashSet::new();
        let mut boundaries = Vec::new();
        for (index, message) in all.iter().enumerate() {
            if pending.is_empty()
                && !matches!(message.role, ChatRole::Tool)
                && (matches!(message.role, ChatRole::User)
                    || latest_user.is_some_and(|user| index > user))
                && index > self.start
            {
                boundaries.push(index);
            }
            for call in message.content.tool_calls() {
                pending.insert(call.call_id.as_str());
            }
            for response in message.content.tool_responses() {
                pending.remove(response.call_id.as_str());
            }
        }
        self.start = self.start.min(all.len());
        request.messages = select(&all, self.start, latest_user);
        // Leave room for the response and estimator error.
        let budget = self.window.saturating_mul(4) / 5;
        for boundary in boundaries {
            if !(force && self.start == old_start) && self.estimated(estimate(request)) <= budget {
                break;
            }
            let selected = select(&all, boundary, latest_user);
            if selected.len() >= request.messages.len() {
                continue;
            }
            self.start = boundary;
            request.messages = selected;
        }
        self.request_estimate = estimate(request);
        self.start > old_start
    }
}

fn select(all: &[ChatMessage], start: usize, latest_user: Option<usize>) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(user) = latest_user.filter(|user| *user < start) {
        messages.push(all[user].clone());
    }
    messages.extend_from_slice(&all[start..]);
    messages
}

fn estimate(request: &ChatRequest) -> usize {
    // Serialized bytes include system text, schemas, signatures and framing.
    serde_json::to_vec(request)
        .map(|bytes| bytes.len().div_ceil(3))
        .unwrap_or(usize::MAX / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{ChatMessage, ToolCall, ToolResponse};
    use serde_json::json;

    #[test]
    fn keeps_system_latest_input_and_whole_roundtrips() {
        let mut request = ChatRequest::new(vec![
            ChatMessage::user("old".repeat(1000)),
            ChatMessage::assistant(vec![ToolCall {
                call_id: "c".into(),
                fn_name: "read".into(),
                fn_arguments: json!({}),
                thought_signatures: None,
            }]),
            ChatMessage::tool(ToolResponse::new("c", "result")),
            ChatMessage::assistant("done"),
            ChatMessage::user("latest"),
        ])
        .with_system("always retained");
        let mut budget = ContextBudget::new(100);
        assert!(budget.prepare(&mut request, false));
        assert_eq!(request.system.as_deref(), Some("always retained"));
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].content.first_text(), Some("latest"));
    }

    #[test]
    fn a_long_single_turn_can_drop_old_roundtrips_not_individual_results() {
        let mut messages = vec![ChatMessage::user("keep this input")];
        for id in ["old", "new"] {
            messages.push(ChatMessage::assistant(vec![ToolCall {
                call_id: id.into(),
                fn_name: "read".into(),
                fn_arguments: json!({}),
                thought_signatures: None,
            }]));
            messages.push(ChatMessage::tool(ToolResponse::new(
                id,
                "large output".repeat(1000),
            )));
        }
        let mut request = ChatRequest::new(messages);
        assert!(ContextBudget::new(100).prepare(&mut request, false));
        assert_eq!(request.messages.len(), 3);
        assert_eq!(
            request.messages[0].content.first_text(),
            Some("keep this input")
        );
        assert_eq!(request.messages[1].content.tool_calls()[0].call_id, "new");
        assert_eq!(
            request.messages[2].content.tool_responses()[0].call_id,
            "new"
        );
    }

    #[test]
    fn measured_usage_scales_with_growth_and_shrinkage() {
        let mut budget = ContextBudget::new(1000);
        let mut request = ChatRequest::new(vec![ChatMessage::user("hello")]);
        budget.prepare(&mut request, false);
        let baseline = budget.request_estimate;
        budget.observe(500);
        assert_eq!(budget.estimated(baseline + 100), 600);
        assert_eq!(budget.estimated(baseline - 1), 499);
        budget.reset();
        assert_eq!(budget.estimated(100), 100);
    }

    #[test]
    fn forced_retry_drops_an_old_turn_but_never_latest_input() {
        let full = ChatRequest::new(vec![
            ChatMessage::user("old"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("new"),
        ]);
        let mut budget = ContextBudget::new(128_000);
        let mut first = full.clone();
        assert!(!budget.prepare(&mut first, false));
        let mut retry = full.clone();
        assert!(budget.prepare(&mut retry, true));
        assert_eq!(retry.messages.len(), 1);
        let mut next = full;
        assert!(!budget.prepare(&mut next, true));
        assert_eq!(next.messages.len(), 1);
    }
}
