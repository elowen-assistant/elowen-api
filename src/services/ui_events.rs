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

use crate::{models::UiEvent, state::AppState};

pub(crate) fn publish_ui_event(state: &AppState, event: UiEvent) {
    publish_ui_event_to(&state.ui_events, event);
}

pub(crate) fn publish_ui_event_to(sender: &broadcast::Sender<UiEvent>, event: UiEvent) {
    let _ = sender.send(event);
}

pub(crate) fn thread_ui_event(thread_id: &str) -> UiEvent {
    UiEvent {
        event_type: "thread.changed".to_string(),
        thread_id: Some(thread_id.to_string()),
        job_id: None,
        device_id: None,
        created_at: Utc::now(),
    }
}

pub(crate) fn job_ui_event(thread_id: &str, job_id: &str, device_id: Option<&str>) -> UiEvent {
    UiEvent {
        event_type: "job.changed".to_string(),
        thread_id: Some(thread_id.to_string()),
        job_id: Some(job_id.to_string()),
        device_id: device_id.map(str::to_string),
        created_at: Utc::now(),
    }
}

pub(crate) fn device_ui_event(device_id: &str) -> UiEvent {
    UiEvent {
        event_type: "device.changed".to_string(),
        thread_id: None,
        job_id: None,
        device_id: Some(device_id.to_string()),
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
