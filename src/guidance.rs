//! Short receiving instructions for request/reply exchanges.
pub const WAITING: &str = "For the expected reply, call check_inbox (CLI: telephone inbox) every 2 seconds for up to 30 seconds or the user's deadline. Stop on the matching reply_to; otherwise report it pending. Do not resend on an empty poll or acknowledge acknowledgments.";
