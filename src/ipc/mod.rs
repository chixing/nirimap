mod client;
mod events;

pub use client::NiriClient;
pub use events::{run_event_loop, validate_and_convert_indices, StateUpdate};
