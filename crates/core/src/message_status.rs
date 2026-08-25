//! Delivery state of an outgoing message.

/// How far an outgoing message has travelled.
///
/// Only meaningful for a message we sent: an incoming one has no delivery
/// state of ours to report, and [`crate::ChatMessage::delivery`] returns
/// `None` for it rather than inventing one.
///
/// The order of the variants is the order of progress, and
/// [`Self::advance`] relies on it. A message that already has a real answer
/// from the server must never be regressed by a later, weaker one: receipts
/// arrive out of order, another of the peer's devices can repeat a delivery
/// ack after the read receipt, and a bubble that flickers from ✓✓ back to ✓
/// reads as a bug in the product rather than in the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum MessageStatus {
    /// Composed locally; the server has not acknowledged it yet.
    #[default]
    Pending,
    /// The server has it.
    Sent,
    /// It reached the recipient's device.
    Delivered,
    /// The recipient opened the conversation, or played the voice note.
    Read,
    /// The send attempt failed. Terminal until the user retries, which starts
    /// a new message rather than moving this one.
    Failed,
}

impl MessageStatus {
    /// Move to `next` unless that would undo progress.
    ///
    /// [`Self::Failed`] is deliberately outside the ordering's meaning: it
    /// sorts last so a failure always lands, and nothing but an explicit reset
    /// leaves it, because a receipt that arrives for a message we already gave
    /// up on refers to an attempt the user was told did not happen.
    pub fn advance(&mut self, next: Self) {
        if *self == Self::Failed {
            return;
        }
        if next > *self {
            *self = next;
        }
    }

    /// Whether the message is still in flight, and so has no tick to draw.
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }

    pub fn is_failed(self) -> bool {
        matches!(self, Self::Failed)
    }

    /// Whether the recipient has seen it — the state the ticks paint in the
    /// accent colour.
    pub fn is_read(self) -> bool {
        matches!(self, Self::Read)
    }

    /// A short label for assistive technology and tooltips, so the state is
    /// never carried by tick colour alone.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Sending",
            Self::Sent => "Sent",
            Self::Delivered => "Delivered",
            Self::Read => "Read",
            Self::Failed => "Not sent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MessageStatus::*;

    #[test]
    fn progress_moves_forward() {
        let mut status = Pending;
        status.advance(Sent);
        assert_eq!(status, Sent);
        status.advance(Delivered);
        assert_eq!(status, Delivered);
        status.advance(Read);
        assert_eq!(status, Read);
    }

    #[test]
    fn a_late_weaker_receipt_cannot_regress_the_bubble() {
        // Another of the peer's devices repeating a delivery ack after the
        // read receipt is the ordinary case, not a corner one.
        let mut status = Read;
        status.advance(Delivered);
        assert_eq!(status, Read);
    }

    #[test]
    fn failure_wins_over_progress_and_then_sticks() {
        let mut status = Sent;
        status.advance(Failed);
        assert_eq!(status, Failed);

        // A receipt for an attempt the user was told had failed must not
        // quietly resurrect it.
        status.advance(Read);
        assert_eq!(status, Failed);
    }

    #[test]
    fn advancing_to_the_same_state_is_a_no_op() {
        let mut status = Delivered;
        status.advance(Delivered);
        assert_eq!(status, Delivered);
    }
}
