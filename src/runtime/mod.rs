pub mod event_sink;

pub(crate) use event_sink::serialize_event;
pub use event_sink::EventSink;

#[cfg(any(test, feature = "e2e-test"))]
pub use event_sink::FakeEventSink;
