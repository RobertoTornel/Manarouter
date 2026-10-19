//! Winner recovery for the hedged race.
//!
//! # What this is for
//!
//! A race commits its winner when the upstream *response* arrives, which is before the body
//! streams. So there is a window in which the committed winner can die: the headers are on the
//! wire, and then the body stops halfway through, or stops before it starts. When that happens the
//! client is left with a truncated answer and no explanation.
//!
//! This module owns the answer to that. It wraps the winner's body so that the relay owns the
//! stream, notices the failure, and — when it is allowed to — continues with a participant that
//! already passed the race.
//!
//! # The rule
//!
//! The replacement is the **highest-priority participant that passed**: it produced a valid reply
//! inside the eligibility window, and its answer is already in the race's result set. Nothing is
//! re-dispatched, so a recovery costs no extra upstream request and no extra latency beyond the
//! time the client spends reading the replacement's body.
//!
//! # The three shapes
//!
//! * **Nothing on the wire yet** — the switch is invisible. The client sees one coherent answer,
//!   the replacement's. Nothing is announced, because there is nothing to reconcile.
//! * **Bytes on the wire, `restart`** — the relay emits the `manarouter.retry` boundary and then
//!   continues with the replacement's body. A client that ignores unknown events still receives a
//!   valid, terminated response.
//! * **Bytes on the wire, `terminate`** — the stream ends with an error. The client decides.
//!
//! # The hard guard
//!
//! Once any tool-call delta has been emitted the relay **refuses to splice**, under both
//! policies. Half an argument from one model plus half an argument from another is a function call
//! that may be perfectly well-formed and completely wrong, and a client that has already seen a
//! tool call opened cannot unsee it. A refused splice ends the stream with an error, which is
//! worse for the answer and much better for the caller's state machine.
//!
//! # What this costs the ordinary path
//!
//! Nothing. The wrapper is only ever installed on the response of a request that was actually in a
//! race, so a request that is not hedged is byte-for-byte what it was before this module existed.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::response::Response;
use bytes::Bytes;
use http_body::Frame;
use mr_core::dialect::{Dialect, MidstreamPolicy};

/// A participant that passed the race and can answer in the winner's place.
///
/// The body is the participant's **client-facing** body, already translated into the client's
/// dialect and already on its way from the upstream. It is unread: the race committed another
/// participant, so this one is only ever pulled if the winner dies.
pub struct Replacement {
    /// How the participant is named in the retry boundary — `provider/model`.
    pub id: String,
    /// The participant's client-facing body, not yet read.
    pub body: AxumBody,
}

/// What the relay does when the winner's body fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Nothing has reached the client: switch without saying anything.
    Silent,
    /// Bytes have reached the client and the policy allows a switch: announce it, then continue.
    Restart,
    /// The switch is not allowed: end the stream with an error.
    Refuse,
}

/// Decide what to do when the winner's body fails.
///
/// The inputs are the three facts that matter: whether anything reached the client, whether a
/// tool-call delta was emitted, and whether there is a participant to fall back to.
///
/// This is a pure function on purpose. It is the whole policy, and a policy that can only be
/// tested through a live HTTP path is a policy that is not tested.
pub fn decide(
    written: u64,
    tool_call: bool,
    has_replacement: bool,
    policy: MidstreamPolicy,
    announceable: bool,
) -> RecoveryAction {
    // The hard guard, under both policies. A false positive here costs a retry; a false negative
    // costs the caller a function call that may be valid and wrong.
    if tool_call {
        return RecoveryAction::Refuse;
    }
    // Nobody to fall back to: the failure is the answer.
    if !has_replacement {
        return RecoveryAction::Refuse;
    }
    // Nothing has reached the client, so the switch is invisible and both policies allow it.
    if written == 0 {
        return RecoveryAction::Silent;
    }
    match policy {
        MidstreamPolicy::Terminate => RecoveryAction::Refuse,
        MidstreamPolicy::Restart => {
            if announceable {
                RecoveryAction::Restart
            } else {
                // The client's body is not an event stream, so there is no frame in which a
                // boundary could be announced — and two documents spliced together are not a
                // document. This is the `terminate` shape with a worse reason.
                RecoveryAction::Refuse
            }
        }
    }
}

/// The frames that mark a mid-stream switch, in the client's dialect.
///
/// The retry event is informational: a client that ignores unknown SSE events — as the
/// specification requires — still receives a valid, terminated response. The frame after it closes
/// the dead model's turn, so a client that is waiting for more content from it stops waiting.
pub fn retry_boundary(client_dialect: Dialect, from: &str, to: &str) -> Vec<Bytes> {
    let data = serde_json::json!({
        "reason": "upstream_failed",
        "from": from,
        "to": to,
    })
    .to_string();
    vec![
        Bytes::from(mr_dialects::sse::named_frame(
            mr_dialects::RETRY_EVENT,
            &data,
        )),
        Bytes::from(terminating_frame(client_dialect)),
    ]
}

/// The frame that ends the dead model's turn, in the client's dialect.
fn terminating_frame(client_dialect: Dialect) -> String {
    match client_dialect {
        // The Anthropic Messages stream ends its turn with `message_stop`.
        Dialect::Anthropic => {
            mr_dialects::sse::named_frame("message_stop", r#"{"type":"message_stop"}"#)
        }
        // The OpenAI Chat Completions stream ends its turn with a final choice carrying
        // `finish_reason` and an empty delta.
        _ => mr_dialects::sse::data_frame(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
    }
}

/// Watches the bytes that reach the client for a tool-call delta.
///
/// The search is a byte-level one on purpose. Parsing every frame as JSON would cost more on the
/// hot path than the guard is worth, and the guard only runs for a request that was in a race. A
/// false positive is safe — the relay refuses to splice — and a false negative is not, so the
/// search is deliberately generous: it looks for the OpenAI spelling and the Anthropic spelling
/// anywhere in the bytes.
///
/// The tail is kept between chunks because a marker can straddle two of them, and a marker that
/// straddles two chunks is exactly the one that would be missed.
pub struct ToolCallGuard {
    /// The last bytes of the previous chunk.
    tail: [u8; ToolCallGuard::TAIL],
    /// How many of them are valid.
    len: usize,
}

impl ToolCallGuard {
    /// How many bytes of the previous chunk are kept. The longest marker is `tool_calls` (ten
    /// bytes), so nine would do; sixteen is comfortably more and costs nothing.
    const TAIL: usize = 16;

    /// The spellings a tool-call delta can have.
    const NEEDLES: [&'static [u8]; 2] = [b"tool_calls", b"tool_use"];

    /// A guard that has seen nothing.
    pub fn new() -> Self {
        Self {
            tail: [0u8; Self::TAIL],
            len: 0,
        }
    }

    /// Feed a chunk of client-facing bytes. Returns `true` if a tool-call marker was seen.
    pub fn observe(&mut self, chunk: &[u8]) -> bool {
        let mut found = false;
        for needle in Self::NEEDLES {
            if contains(chunk, needle) || straddles(&self.tail[..self.len], chunk, needle) {
                found = true;
            }
        }

        // Keep the tail of this chunk for the next one.
        let keep = Self::TAIL.min(chunk.len());
        self.tail[..keep].copy_from_slice(&chunk[chunk.len() - keep..]);
        self.len = keep;
        found
    }
}

impl Default for ToolCallGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Whether `needle` straddles the boundary between `tail` and `chunk`.
///
/// The marker may be split anywhere: the first *k* bytes of the marker are the last *k* bytes of
/// the tail, and the rest are the first bytes of the chunk.
fn straddles(tail: &[u8], chunk: &[u8], needle: &[u8]) -> bool {
    let max = needle.len().saturating_sub(1).min(tail.len());
    for k in 1..=max {
        if tail[tail.len() - k..] != needle[..k] {
            continue;
        }
        let rest = &needle[k..];
        if chunk.len() >= rest.len() && &chunk[..rest.len()] == rest {
            return true;
        }
    }
    false
}

/// Which body is being streamed right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The committed winner.
    Winner,
    /// A replacement, by index into the replacement list.
    Replacement(usize),
}

/// The winner's body, watched.
///
/// It forwards every chunk unchanged, notices when the upstream body errors, remembers whether
/// anything has reached the client, and — when the policy allows it — continues with a
/// participant that passed the race.
pub struct RecoveryBody {
    /// The committed winner's client-facing body.
    winner: AxumBody,
    /// The participants that passed the race, in priority order.
    replacements: Vec<Replacement>,
    /// The next replacement to try.
    next: usize,
    /// The combo's mid-stream policy.
    policy: MidstreamPolicy,
    /// The dialect the client speaks.
    client_dialect: Dialect,
    /// The id of the participant whose body is being streamed.
    current_id: String,
    /// The id of the committed winner.
    winner_id: String,
    /// How many bytes have reached the client.
    written: u64,
    /// Whether a tool-call delta has been seen.
    tool_call: bool,
    /// The guard that spots a tool-call delta.
    guard: ToolCallGuard,
    /// The frames waiting to be written — the retry boundary.
    pending: VecDeque<Bytes>,
    /// Whether the client-facing body is an event stream, and can therefore carry a boundary.
    announceable: bool,
    /// Whether the body has ended.
    ended: bool,
    /// Which body is being streamed.
    source: Source,
}

impl RecoveryBody {
    /// Build the wrapper.
    pub fn new(
        winner: AxumBody,
        replacements: Vec<Replacement>,
        policy: MidstreamPolicy,
        client_dialect: Dialect,
        winner_id: String,
        announceable: bool,
    ) -> Self {
        Self {
            current_id: winner_id.clone(),
            winner,
            replacements,
            next: 0,
            policy,
            client_dialect,
            winner_id,
            written: 0,
            tool_call: false,
            guard: ToolCallGuard::new(),
            pending: VecDeque::new(),
            announceable,
            ended: false,
            source: Source::Winner,
        }
    }

    /// Whether there is still a participant to fall back to.
    fn has_replacement(&self) -> bool {
        self.next < self.replacements.len()
    }

    /// Note the bytes that reached the client, and look for a tool-call delta in them.
    fn observe(&mut self, frame: &Frame<Bytes>) {
        let Some(data) = frame.data_ref() else {
            return;
        };
        self.written += data.len() as u64;
        if self.guard.observe(data) {
            self.tool_call = true;
        }
    }

    /// The winner's body failed. Decide what to do, and set up the next body.
    ///
    /// `from` is the participant whose body just died: the winner, or a replacement that failed in
    /// its turn.
    fn fail(&mut self, err: axum::Error, from: &str) -> Result<(), axum::Error> {
        let action = decide(
            self.written,
            self.tool_call,
            self.has_replacement(),
            self.policy,
            self.announceable,
        );
        match action {
            RecoveryAction::Refuse => {
                let reason = if self.tool_call {
                    "a tool-call delta was already emitted, and splicing two models' tool calls \
                     would assemble a call that may be valid and wrong"
                } else if !self.has_replacement() {
                    "no participant passed the race that could answer in the winner's place"
                } else {
                    match self.policy {
                        MidstreamPolicy::Terminate => {
                            "the combo's mid-stream policy is terminate, and bytes were already \
                             on the wire"
                        }
                        MidstreamPolicy::Restart => {
                            "the client's body is not an event stream, so a switch cannot be \
                             announced and two documents cannot be spliced"
                        }
                    }
                };
                tracing::warn!(
                    from,
                    written = self.written,
                    tool_call = self.tool_call,
                    "the winning model's stream failed and the request could not be recovered"
                );
                Err(axum::Error::new(std::io::Error::other(format!(
                    "the upstream stream failed and the request could not be recovered: {reason}: \
                     {err}"
                ))))
            }
            RecoveryAction::Silent => {
                let index = self.next;
                self.next += 1;
                self.source = Source::Replacement(index);
                self.current_id = self.replacements[index].id.clone();
                tracing::warn!(
                    from,
                    to = %self.current_id,
                    "the upstream stream failed before anything reached the client; continuing \
                     with the participant that passed"
                );
                Ok(())
            }
            RecoveryAction::Restart => {
                let index = self.next;
                self.next += 1;
                let to = self.replacements[index].id.clone();
                self.pending
                    .extend(retry_boundary(self.client_dialect, from, &to));
                self.source = Source::Replacement(index);
                self.current_id = to;
                tracing::warn!(
                    from,
                    to = %self.current_id,
                    "the upstream stream failed mid-stream; continuing with the participant \
                     that passed"
                );
                Ok(())
            }
        }
    }
}

impl http_body::Body for RecoveryBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let me = self.get_mut();
        loop {
            if me.ended {
                return Poll::Ready(None);
            }

            // The boundary goes out before the replacement's first byte: it is what tells the
            // client that the model it was talking to just changed.
            if let Some(frame) = me.pending.pop_front() {
                me.written += frame.len() as u64;
                return Poll::Ready(Some(Ok(Frame::data(frame))));
            }

            match me.source {
                Source::Winner => match Pin::new(&mut me.winner).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => {
                        me.observe(&frame);
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    Poll::Ready(Some(Err(err))) => {
                        let from = me.winner_id.clone();
                        match me.fail(err, &from) {
                            Ok(()) => continue,
                            Err(err) => {
                                me.ended = true;
                                return Poll::Ready(Some(Err(err)));
                            }
                        }
                    }
                    Poll::Ready(None) => {
                        me.ended = true;
                        return Poll::Ready(None);
                    }
                    Poll::Pending => return Poll::Pending,
                },
                Source::Replacement(index) => {
                    match Pin::new(&mut me.replacements[index].body).poll_frame(cx) {
                        Poll::Ready(Some(Ok(frame))) => {
                            me.observe(&frame);
                            return Poll::Ready(Some(Ok(frame)));
                        }
                        Poll::Ready(Some(Err(err))) => {
                            // The replacement died too: fall through to the next participant that
                            // passed, in priority order.
                            let from = me.current_id.clone();
                            match me.fail(err, &from) {
                                Ok(()) => continue,
                                Err(err) => {
                                    me.ended = true;
                                    return Poll::Ready(Some(Err(err)));
                                }
                            }
                        }
                        Poll::Ready(None) => {
                            me.ended = true;
                            return Poll::Ready(None);
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }

    fn size_hint(&self) -> http_body::SizeHint {
        // A splice changes the length of the answer, so the winner's length is only trustworthy
        // when there is nothing to splice — and then the answer is exactly what it was.
        match self.source {
            Source::Winner if self.replacements.is_empty() => self.winner.size_hint(),
            _ => http_body::SizeHint::default(),
        }
    }
}

/// Wrap a committed winner's response so that a mid-stream failure can be recovered.
///
/// The response's headers are the winner's and stay the winner's: they are on the wire by the time
/// the body streams, and a replacement is not allowed to change them.
pub fn wrap(
    response: Response,
    replacements: Vec<Replacement>,
    policy: MidstreamPolicy,
    client_dialect: Dialect,
    winner_id: String,
) -> Response {
    let (parts, body) = response.into_parts();

    // Whether the client's body is an event stream. It decides whether a switch can be announced:
    // the boundary is an SSE frame, and a JSON document cannot carry one. A body whose type is
    // unknown is given the benefit of the doubt — the relay sets the type itself on the translated
    // path, and an upstream that streams without saying so is streaming events.
    let announceable = match parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => {
            let value = value.to_ascii_lowercase();
            value.contains("text/event-stream") || !value.contains("json")
        }
        None => true,
    };

    let body = RecoveryBody::new(
        body,
        replacements,
        policy,
        client_dialect,
        winner_id,
        announceable,
    );
    Response::from_parts(parts, AxumBody::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> ToolCallGuard {
        ToolCallGuard::new()
    }

    #[test]
    fn nothing_on_the_wire_switches_silently_under_both_policies() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            assert_eq!(
                decide(0, false, true, policy, true),
                RecoveryAction::Silent,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn bytes_on_the_wire_follow_the_policy() {
        assert_eq!(
            decide(10, false, true, MidstreamPolicy::Restart, true),
            RecoveryAction::Restart
        );
        assert_eq!(
            decide(10, false, true, MidstreamPolicy::Terminate, true),
            RecoveryAction::Refuse
        );
    }

    #[test]
    fn a_tool_call_delta_refuses_the_splice_under_both_policies() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            assert_eq!(
                decide(0, true, true, policy, true),
                RecoveryAction::Refuse,
                "no bytes on the wire, but a tool call was emitted: {policy:?}"
            );
            assert_eq!(
                decide(10, true, true, policy, true),
                RecoveryAction::Refuse,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn no_passed_participant_means_no_recovery() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            assert_eq!(
                decide(0, false, false, policy, true),
                RecoveryAction::Refuse,
                "{policy:?}"
            );
            assert_eq!(
                decide(10, false, false, policy, true),
                RecoveryAction::Refuse,
                "{policy:?}"
            );
        }
    }

    #[test]
    fn a_body_that_is_not_an_event_stream_cannot_carry_the_boundary() {
        assert_eq!(
            decide(10, false, true, MidstreamPolicy::Restart, false),
            RecoveryAction::Refuse
        );
        // Nothing on the wire is still an invisible switch: the client sees one document.
        assert_eq!(
            decide(0, false, true, MidstreamPolicy::Restart, false),
            RecoveryAction::Silent
        );
    }

    #[test]
    fn the_guard_finds_the_openai_spelling() {
        let mut g = guard();
        assert!(g.observe(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[]}}]}"));
    }

    #[test]
    fn the_guard_finds_the_anthropic_spelling() {
        let mut g = guard();
        assert!(g.observe(b"event: content_block_start\ndata: {\"type\":\"tool_use\"}"));
    }

    #[test]
    fn the_guard_ignores_an_ordinary_answer() {
        let mut g = guard();
        assert!(!g.observe(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}"));
        assert!(!g.observe(b"data: [DONE]"));
    }

    #[test]
    fn the_guard_finds_a_marker_split_across_two_chunks() {
        let mut g = guard();
        assert!(!g.observe(b"data: {\"delta\":{\"tool_"));
        assert!(g.observe(b"calls\":[]}}"));
    }

    #[test]
    fn the_guard_finds_a_marker_split_at_every_point() {
        for split in 1.."tool_calls".len() {
            let mut g = guard();
            let (head, tail) = "tool_calls".split_at(split);
            assert!(!g.observe(head.as_bytes()), "split at {split}");
            assert!(g.observe(tail.as_bytes()), "split at {split}");
        }
    }

    #[test]
    fn the_boundary_names_the_dead_model_and_the_replacement() {
        let frames = retry_boundary(Dialect::OpenAi, "openai/a", "openai/b");
        assert_eq!(frames.len(), 2);

        let retry = String::from_utf8(frames[0].to_vec()).unwrap();
        assert!(retry.starts_with("event: manarouter.retry\n"), "{retry}");
        assert!(retry.contains(r#""reason":"upstream_failed""#), "{retry}");
        assert!(retry.contains(r#""from":"openai/a""#), "{retry}");
        assert!(retry.contains(r#""to":"openai/b""#), "{retry}");

        let terminator = String::from_utf8(frames[1].to_vec()).unwrap();
        assert_eq!(
            terminator,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        );
    }

    #[test]
    fn the_boundary_terminates_in_the_clients_dialect() {
        let frames = retry_boundary(Dialect::Anthropic, "anthropic/a", "anthropic/b");
        let terminator = String::from_utf8(frames[1].to_vec()).unwrap();
        assert_eq!(
            terminator,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }
}
