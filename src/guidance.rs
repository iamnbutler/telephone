//! Agent-facing return-path instructions, independent of the outbound transport.
use crate::{
    address::{shell_quote, Address},
    store::registrations::Registration,
};
use serde::Serialize;
use uuid::Uuid;

const DELIVERY: &str = "Incoming messages require polling. Telephone will not wake this thread, even when your outgoing message used native delivery.";
const WAITING: &str = "When a user-authorized exchange expects a reply, poll your own inbox before ending the exchange; do not wait for the user to remind you. Check every 2 seconds for up to 30 seconds, or use the user's deadline. Stop when the expected reply arrives; otherwise report it pending. An empty poll is not a delivery failure. Do not resend because the inbox is empty or acknowledge acknowledgments.";

#[derive(Serialize)]
struct InboxArguments<'a> {
    address: &'a Address,
}

#[derive(Serialize)]
struct InboxCall<'a> {
    tool: &'static str,
    arguments: InboxArguments<'a>,
}

#[derive(Serialize)]
pub struct Polling<'a> {
    delivery: &'static str,
    check_inbox: InboxCall<'a>,
    cli: String,
    instructions: &'static str,
}

impl<'a> Polling<'a> {
    pub fn new(address: &'a Address) -> Self {
        Self {
            delivery: DELIVERY,
            check_inbox: InboxCall {
                tool: "check_inbox",
                arguments: InboxArguments { address },
            },
            cli: format!(
                "TELEPHONE_ADDR={} telephone inbox",
                shell_quote(address.as_str())
            ),
            instructions: WAITING,
        }
    }

    pub fn text(&self, reply_to: Option<Uuid>) -> String {
        let address = self.check_inbox.arguments.address;
        let mut text = format!(
            "{}\nMCP: call check_inbox with {}\nCLI: {}\n{}",
            self.delivery,
            serde_json::json!({"address":address}),
            self.cli,
            self.instructions
        );
        if let Some(id) = reply_to {
            text.push_str(&format!(
                "\nMatch the expected reply by reply_to (rendered as 'in reply to'): {id}."
            ));
        }
        text
    }
}

#[derive(Serialize)]
pub struct RegistrationReport<'a> {
    #[serde(flatten)]
    registration: &'a Registration,
    receiving: Polling<'a>,
}

impl<'a> RegistrationReport<'a> {
    pub fn new(registration: &'a Registration) -> Self {
        Self {
            registration,
            receiving: Polling::new(&registration.address),
        }
    }
}
