//! UI event fanout helpers.

use axum::response::{
    Sse,
    sse::{Event, KeepAlive},
};
use chrono::Utc;
use futures_util::Stream;
use std::convert::Infallible;
use tokio::sync::broadcast;
use tracing::warn;
use ulid::Ulid;

use crate::{models::UiEvent, state::AppState};

pub(crate) fn publish_ui_event(state: &AppState, event: UiEvent) {
    publish_ui_event_to(&state.ui_events, event);
}

pub(crate) fn publish_ui_event_to(sender: &broadcast::Sender<UiEvent>, event: UiEvent) {
    let _ = sender.send(event);
}

pub(crate) fn thread_ui_event(thread_id: &str) -> UiEvent {
    UiEvent {
        event_id: Ulid::new().to_string(),
        event_type: "thread.changed".to_string(),
        resource_kind: "thread".to_string(),
        resource_id: Some(thread_id.to_string()),
        action: "changed".to_string(),
        thread_id: Some(thread_id.to_string()),
        job_id: None,
        device_id: None,
        created_at: Utc::now(),
    }
}

pub(crate) fn job_ui_event(thread_id: &str, job_id: &str, device_id: Option<&str>) -> UiEvent {
    UiEvent {
        event_id: Ulid::new().to_string(),
        event_type: "job.changed".to_string(),
        resource_kind: "job".to_string(),
        resource_id: Some(job_id.to_string()),
        action: "changed".to_string(),
        thread_id: Some(thread_id.to_string()),
        job_id: Some(job_id.to_string()),
        device_id: device_id.map(str::to_string),
        created_at: Utc::now(),
    }
}

pub(crate) fn device_ui_event(device_id: &str) -> UiEvent {
    UiEvent {
        event_id: Ulid::new().to_string(),
        event_type: "device.changed".to_string(),
        resource_kind: "device".to_string(),
        resource_id: Some(device_id.to_string()),
        action: "changed".to_string(),
        thread_id: None,
        job_id: None,
        device_id: Some(device_id.to_string()),
        created_at: Utc::now(),
    }
}

pub(crate) fn signer_ui_event(key_id: &str) -> UiEvent {
    UiEvent {
        event_id: Ulid::new().to_string(),
        event_type: "trust.signers.changed".to_string(),
        resource_kind: "trust.signer".to_string(),
        resource_id: Some(key_id.to_string()),
        action: "changed".to_string(),
        thread_id: None,
        job_id: None,
        device_id: None,
        created_at: Utc::now(),
    }
}

pub(crate) fn stream_from_broadcast(
    mut receiver: broadcast::Receiver<UiEvent>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let event_type = event.event_type.clone();
                    match serde_json::to_string(&event) {
                        Ok(payload) => {
                            yield Ok(Event::default().event(event_type).data(payload));
                        }
                        Err(error) => {
                            warn!(error = %error, "failed to serialize UI event");
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "UI event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::{device_ui_event, job_ui_event, signer_ui_event, thread_ui_event};

    #[test]
    fn ui_events_include_catch_up_envelope_fields() {
        let thread = thread_ui_event("thread-1");
        assert!(!thread.event_id.is_empty());
        assert_eq!(thread.event_type, "thread.changed");
        assert_eq!(thread.resource_kind, "thread");
        assert_eq!(thread.resource_id.as_deref(), Some("thread-1"));
        assert_eq!(thread.action, "changed");

        let job = job_ui_event("thread-1", "job-1", Some("edge-1"));
        assert!(!job.event_id.is_empty());
        assert_eq!(job.resource_kind, "job");
        assert_eq!(job.resource_id.as_deref(), Some("job-1"));
        assert_eq!(job.device_id.as_deref(), Some("edge-1"));

        let device = device_ui_event("edge-1");
        assert_eq!(device.resource_kind, "device");
        assert_eq!(device.resource_id.as_deref(), Some("edge-1"));

        let signer = signer_ui_event("signer-1");
        assert_eq!(signer.event_type, "trust.signers.changed");
        assert_eq!(signer.resource_kind, "trust.signer");
        assert_eq!(signer.resource_id.as_deref(), Some("signer-1"));
    }
}
