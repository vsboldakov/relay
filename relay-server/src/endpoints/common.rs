//! Common facilities for ingesting events through store-like endpoints.

use std::cell::RefCell;
use std::rc::Rc;

use actix::prelude::*;
use actix_web::http::{header, StatusCode};
use actix_web::middleware::cors::{Cors, CorsBuilder};
use actix_web::{HttpRequest, HttpResponse, ResponseError};
use failure::Fail;
use futures::prelude::*;
use serde::Deserialize;

use relay_common::{clone, tryf};
use relay_config::Config;
use relay_general::protocol::{EventId, EventType};
use relay_log::LogError;
use relay_quotas::RateLimits;
use relay_statsd::metric;

use crate::actors::envelopes::{
    EnvelopeContext, EnvelopeManager, QueueEnvelope, QueueEnvelopeError,
};
use crate::actors::outcome::{DiscardReason, Outcome};
use crate::actors::project_cache::{CheckEnvelope, ProjectCache, ProjectError};
use crate::body::StorePayloadError;
use crate::envelope::{AttachmentType, Envelope, EnvelopeError, ItemType, Items};
use crate::extractors::RequestMeta;
use crate::service::{ServiceApp, ServiceState};
use crate::statsd::RelayCounters;
use crate::utils::{self, ApiErrorResponse, FormDataIter, MultipartError, SendWithOutcome};

#[derive(Fail, Debug)]
pub enum BadStoreRequest {
    #[fail(display = "unsupported protocol version ({})", _0)]
    UnsupportedProtocolVersion(u16),

    #[fail(display = "could not schedule event processing")]
    ScheduleFailed,

    #[fail(display = "failed to fetch project information")]
    ProjectFailed(#[cause] ProjectError),

    #[fail(display = "empty request body")]
    EmptyBody,

    #[fail(display = "empty envelope")]
    EmptyEnvelope,

    #[fail(display = "invalid JSON data")]
    InvalidJson(#[cause] serde_json::Error),

    #[fail(display = "invalid messagepack data")]
    InvalidMsgpack(#[cause] rmp_serde::decode::Error),

    #[fail(display = "invalid event envelope")]
    InvalidEnvelope(#[cause] EnvelopeError),

    #[fail(display = "invalid multipart data")]
    InvalidMultipart(#[cause] MultipartError),

    #[fail(display = "invalid minidump")]
    InvalidMinidump,

    #[fail(display = "missing minidump")]
    MissingMinidump,

    #[fail(display = "invalid unreal crash report")]
    InvalidUnrealReport,

    #[fail(display = "invalid event id")]
    InvalidEventId,

    #[fail(display = "failed to queue envelope")]
    QueueFailed(#[cause] QueueEnvelopeError),

    #[fail(display = "failed to read request body")]
    PayloadError(#[cause] StorePayloadError),

    #[fail(display = "event rejected due to rate limit")]
    RateLimited(RateLimits),

    #[fail(display = "event submission rejected with_reason: {:?}", _0)]
    EventRejected(DiscardReason),
}

impl BadStoreRequest {
    fn to_outcome(&self) -> Option<Outcome> {
        Some(match self {
            BadStoreRequest::UnsupportedProtocolVersion(_) => {
                Outcome::Invalid(DiscardReason::AuthVersion)
            }

            BadStoreRequest::InvalidUnrealReport => {
                Outcome::Invalid(DiscardReason::MissingMinidumpUpload)
            }

            BadStoreRequest::EmptyBody => Outcome::Invalid(DiscardReason::NoData),
            BadStoreRequest::EmptyEnvelope => Outcome::Invalid(DiscardReason::EmptyEnvelope),
            BadStoreRequest::InvalidJson(_) => Outcome::Invalid(DiscardReason::InvalidJson),
            BadStoreRequest::InvalidMsgpack(_) => Outcome::Invalid(DiscardReason::InvalidMsgpack),
            BadStoreRequest::InvalidMultipart(_) => {
                Outcome::Invalid(DiscardReason::InvalidMultipart)
            }
            BadStoreRequest::InvalidMinidump => Outcome::Invalid(DiscardReason::InvalidMinidump),
            BadStoreRequest::MissingMinidump => {
                Outcome::Invalid(DiscardReason::MissingMinidumpUpload)
            }
            BadStoreRequest::InvalidEnvelope(_) => Outcome::Invalid(DiscardReason::InvalidEnvelope),

            BadStoreRequest::QueueFailed(event_error) => match event_error {
                QueueEnvelopeError::TooManyEnvelopes => Outcome::Invalid(DiscardReason::Internal),
            },
            BadStoreRequest::ProjectFailed(project_error) => match project_error {
                ProjectError::FetchFailed => Outcome::Invalid(DiscardReason::ProjectState),
                _ => Outcome::Invalid(DiscardReason::Internal),
            },
            BadStoreRequest::PayloadError(payload_error) => {
                Outcome::Invalid(payload_error.discard_reason())
            }

            // should actually never create an outcome
            BadStoreRequest::InvalidEventId => Outcome::Invalid(DiscardReason::Internal),

            // Outcomes emitted at the source
            BadStoreRequest::EventRejected(_) => return None,
            BadStoreRequest::RateLimited(_) => return None,
            BadStoreRequest::ScheduleFailed => return None,
        })
    }
}

impl ResponseError for BadStoreRequest {
    fn error_response(&self) -> HttpResponse {
        let body = ApiErrorResponse::from_fail(self);

        match self {
            BadStoreRequest::RateLimited(rate_limits) => {
                let retry_after_header = rate_limits
                    .longest()
                    .map(|limit| limit.retry_after.remaining_seconds().to_string())
                    .unwrap_or_default();

                let rate_limits_header = utils::format_rate_limits(rate_limits);

                // For rate limits, we return a special status code and indicate the client to hold
                // off until the rate limit period has expired. Currently, we only support the
                // delay-seconds variant of the Rate-Limit header.
                HttpResponse::build(StatusCode::TOO_MANY_REQUESTS)
                    .header(header::RETRY_AFTER, retry_after_header)
                    .header(utils::RATE_LIMITS_HEADER, rate_limits_header)
                    .json(&body)
            }
            BadStoreRequest::ProjectFailed(project_error) => match project_error {
                ProjectError::FetchFailed => {
                    // This particular project is somehow broken. We could treat this as 503 but it's
                    // more likely that the error is local to this project.
                    HttpResponse::InternalServerError().json(&body)
                }
                ProjectError::ScheduleFailed => HttpResponse::ServiceUnavailable().json(&body),
            },

            BadStoreRequest::ScheduleFailed | BadStoreRequest::QueueFailed(_) => {
                // These errors indicate that something's wrong with our actor system, most likely
                // mailbox congestion or a faulty shutdown. Indicate an unavailable service to the
                // client. It might retry event submission at a later time.
                HttpResponse::ServiceUnavailable().json(&body)
            }
            BadStoreRequest::EventRejected(_) => {
                // The event has been discarded, which is generally indicated with a 403 error.
                // Originally, Sentry also used this status code for event filters, but these are
                // now executed asynchronously in `EnvelopeProcessor`.
                HttpResponse::Forbidden().json(&body)
            }
            BadStoreRequest::PayloadError(StorePayloadError::Overflow) => {
                HttpResponse::PayloadTooLarge().json(&body)
            }
            _ => {
                // In all other cases, we indicate a generic bad request to the client and render
                // the cause. This was likely the client's fault.
                HttpResponse::BadRequest().json(&body)
            }
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct MinimalEvent {
    #[serde(default, rename = "event_id")]
    pub id: Option<EventId>,

    #[serde(default, rename = "type")]
    pub ty: EventType,
}

/// Parses a minimal subset of the event payload.
///
/// This function validates that the provided payload is valid and returns an `Err` on parse errors.
pub fn minimal_event_from_json(data: &[u8]) -> Result<MinimalEvent, BadStoreRequest> {
    serde_json::from_slice(data).map_err(BadStoreRequest::InvalidJson)
}

/// Extracts the event id from a JSON payload.
///
/// If the payload contains no event id, `Ok(None)` is returned. This function also validates that
/// the provided is valid and returns an `Err` on parse errors. If the event id itself is malformed,
/// an `Err` is returned.
pub fn event_id_from_json(data: &[u8]) -> Result<Option<EventId>, BadStoreRequest> {
    minimal_event_from_json(data).map(|event| event.id)
}

/// Extracts the event id from a MessagePack payload.
///
/// If the payload contains no event id, `Ok(None)` is returned. This function also validates that
/// the provided is valid and returns an `Err` on parse errors. If the event id itself is malformed,
/// an `Err` is returned.
pub fn event_id_from_msgpack(data: &[u8]) -> Result<Option<EventId>, BadStoreRequest> {
    rmp_serde::from_slice(data)
        .map(|MinimalEvent { id, .. }| id)
        .map_err(BadStoreRequest::InvalidMsgpack)
}

/// Extracts the event id from `sentry` JSON payload or the `sentry[event_id]` formdata key.
///
/// If the event id itself is malformed, an `Err` is returned. If there is a `sentry` key containing
/// malformed JSON, an error is returned.
pub fn event_id_from_formdata(data: &[u8]) -> Result<Option<EventId>, BadStoreRequest> {
    for entry in FormDataIter::new(data) {
        if entry.key() == "sentry" {
            return event_id_from_json(entry.value().as_bytes());
        } else if entry.key() == "sentry[event_id]" {
            return entry
                .value()
                .parse()
                .map(Some)
                .map_err(|_| BadStoreRequest::InvalidEventId);
        }
    }

    Ok(None)
}

/// Extracts the event id from multiple items.
///
/// Submitting multiple event payloads is undefined behavior. This function will check for an event
/// id in the following precedence:
///
///  1. The `Event` item.
///  2. The `__sentry-event` event attachment.
///  3. The `sentry` JSON payload.
///  4. The `sentry[event_id]` formdata key.
///
/// # Limitations
///
/// Extracting the event id from chunked formdata fields on the Minidump endpoint (`sentry__1`,
/// `sentry__2`, ...) is not supported. In this case, `None` is returned.
pub fn event_id_from_items(items: &Items) -> Result<Option<EventId>, BadStoreRequest> {
    if let Some(item) = items.iter().find(|item| item.ty() == ItemType::Event) {
        if let Some(event_id) = event_id_from_json(&item.payload())? {
            return Ok(Some(event_id));
        }
    }

    if let Some(item) = items
        .iter()
        .find(|item| item.attachment_type() == Some(AttachmentType::EventPayload))
    {
        if let Some(event_id) = event_id_from_msgpack(&item.payload())? {
            return Ok(Some(event_id));
        }
    }

    if let Some(item) = items.iter().find(|item| item.ty() == ItemType::FormData) {
        // Swallow all other errors here since it is quite common to receive invalid secondary
        // payloads. `EnvelopeProcessor` also retains events in such cases.
        if let Ok(Some(event_id)) = event_id_from_formdata(&item.payload()) {
            return Ok(Some(event_id));
        }
    }

    Ok(None)
}

/// Creates a preconfigured CORS middleware builder for store requests.
///
/// To configure CORS, register endpoints using `resource()` and finalize by calling `register()`, which
/// returns an App. This configures POST as allowed method, allows default sentry headers, and
/// exposes the return headers.
pub fn cors(app: ServiceApp) -> CorsBuilder<ServiceState> {
    let mut builder = Cors::for_app(app);

    builder
        .allowed_methods(vec!["POST"])
        .allowed_headers(vec![
            "x-sentry-auth",
            "x-requested-with",
            "x-forwarded-for",
            "origin",
            "referer",
            "accept",
            "content-type",
            "authentication",
            "authorization",
            "content-encoding",
            "transfer-encoding",
        ])
        .expose_headers(vec![
            "x-sentry-error",
            "x-sentry-rate-limits",
            "retry-after",
        ])
        .max_age(3600);

    builder
}

/// Checks for size limits of items in this envelope.
///
/// Returns `true`, if the envelope adheres to the configured size limits. Otherwise, returns
/// `false`, in which case the envelope should be discarded and a `413 Payload Too Large` response
/// shoult be given.
///
/// The following limits are checked:
///
///  - `max_event_size`
///  - `max_attachment_size`
///  - `max_attachments_size`
///  - `max_session_count`
fn check_envelope_size_limits(config: &Config, envelope: &Envelope) -> bool {
    let mut event_size = 0;
    let mut attachments_size = 0;
    let mut session_count = 0;
    let mut client_reports_size = 0;

    for item in envelope.items() {
        match item.ty() {
            ItemType::Event
            | ItemType::Transaction
            | ItemType::Security
            | ItemType::RawSecurity
            | ItemType::FormData => event_size += item.len(),
            ItemType::Attachment | ItemType::UnrealReport => {
                if item.len() > config.max_attachment_size() {
                    return false;
                }

                attachments_size += item.len()
            }
            ItemType::Session => session_count += 1,
            ItemType::Sessions => session_count += 1,
            ItemType::UserReport => (),
            ItemType::Metrics => (),
            ItemType::MetricBuckets => (),
            ItemType::ClientReport => client_reports_size += item.len(),
        }
    }

    event_size <= config.max_event_size()
        && attachments_size <= config.max_attachments_size()
        && session_count <= config.max_session_count()
        && client_reports_size <= config.max_client_reports_size()
}

/// Handles Sentry events.
///
/// Sentry events may come either directly from a http request ( the store endpoint calls this
/// method directly) or are generated inside Relay from requests to other endpoints (e.g. the
/// security endpoint)
///
/// If store_event receives a non empty store_body it will use it as the body of the event otherwise
/// it will try to create a store_body from the request.
pub fn handle_store_like_request<F, R, I>(
    meta: RequestMeta,
    request: HttpRequest<ServiceState>,
    extract_envelope: F,
    create_response: R,
    emit_rate_limit: bool,
) -> ResponseFuture<HttpResponse, BadStoreRequest>
where
    F: FnOnce(&HttpRequest<ServiceState>, RequestMeta) -> I + 'static,
    I: IntoFuture<Item = Envelope, Error = BadStoreRequest> + 'static,
    R: FnOnce(Option<EventId>) -> HttpResponse + Copy + 'static,
{
    // For now, we only handle <= v8 and drop everything else
    let version = meta.version();
    if version > relay_common::PROTOCOL_VERSION {
        // TODO: Delegate to forward_upstream here
        tryf!(Err(BadStoreRequest::UnsupportedProtocolVersion(version)));
    }

    metric!(
        counter(RelayCounters::EventProtocol) += 1,
        version = &format!("{}", version)
    );

    let project_key = meta.public_key();
    let start_time = meta.start_time();
    let config = request.state().config();

    let envelope_context = Rc::new(RefCell::new(EnvelopeContext::from_request(&meta)));

    let future = extract_envelope(&request, meta)
        .into_future()
        .and_then(clone!(envelope_context, |envelope| {
            envelope_context.borrow_mut().update(&envelope);
            if envelope.is_empty() {
                // envelope is empty, cannot send outcomes
                Err(BadStoreRequest::EmptyEnvelope)
            } else {
                Ok(envelope)
            }
        }))
        .and_then(clone!(envelope_context, |envelope| {
            ProjectCache::from_registry()
                .send_tracked(
                    CheckEnvelope::cached(project_key, envelope),
                    *envelope_context.clone().borrow(),
                )
                .map_err(|_| BadStoreRequest::ScheduleFailed)
                .and_then(|result| result.map_err(BadStoreRequest::ProjectFailed))
                .map_err(move |err| {
                    if let Some(outcome) = err.to_outcome() {
                        envelope_context.borrow().send_outcomes(outcome);
                    }
                    err
                })
        }))
        .and_then(clone!(envelope_context, |response| {
            let mut envelope_context = envelope_context.borrow_mut();
            envelope_context.scope(response.scoping);

            let checked = response.result.map_err(|reason| {
                envelope_context.send_outcomes(Outcome::Invalid(reason));
                BadStoreRequest::EventRejected(reason)
            })?;

            // Skip over queuing and issue a rate limit right away
            let envelope = match checked.envelope {
                Some(envelope) => envelope,
                // rate limit outcome logged by CheckEnvelope already
                None => return Err(BadStoreRequest::RateLimited(checked.rate_limits)),
            };

            envelope_context.update(&envelope);
            if check_envelope_size_limits(&config, &envelope) {
                Ok((envelope, checked.rate_limits))
            } else {
                envelope_context.send_outcomes(Outcome::Invalid(DiscardReason::TooLarge));
                Err(BadStoreRequest::PayloadError(StorePayloadError::Overflow))
            }
        }))
        .and_then(clone!(envelope_context, |(envelope, rate_limits)| {
            let message = QueueEnvelope {
                envelope,
                project_key,
                start_time,
            };

            EnvelopeManager::from_registry()
                .send_tracked(message, *envelope_context.clone().borrow())
                .map_err(|_| BadStoreRequest::ScheduleFailed)
                .and_then(|result| result.map_err(BadStoreRequest::QueueFailed))
                .map_err(move |err| {
                    if let Some(outcome) = err.to_outcome() {
                        // TODO: Move this into Handler<QueueEnvelope>
                        envelope_context.borrow().send_outcomes(outcome)
                    }
                    err
                })
                .map(move |event_id| (event_id, rate_limits))
        }))
        .and_then(move |(event_id, rate_limits)| {
            if rate_limits.is_limited() {
                Err(BadStoreRequest::RateLimited(rate_limits))
            } else {
                Ok(create_response(event_id))
            }
        })
        .or_else(move |error: BadStoreRequest| {
            metric!(counter(RelayCounters::EnvelopeRejected) += 1);
            let event_id = envelope_context.borrow().event_id();

            if !emit_rate_limit && matches!(error, BadStoreRequest::RateLimited(_)) {
                return Ok(create_response(event_id));
            }

            let response = error.error_response();
            if response.status().is_server_error() {
                relay_log::error!("error handling request: {}", LogError(&error));
            }

            Ok(response)
        });

    Box::new(future)
}

/// Creates a HttpResponse containing the textual representation of the given EventId
pub fn create_text_event_id_response(id: Option<EventId>) -> HttpResponse {
    // Event id is set statically in the ingest path.
    let id = id.unwrap_or_default();
    debug_assert!(!id.is_nil());

    // the minidump client expects the response to contain an event id as a hyphenated UUID
    // i.e. xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    HttpResponse::Ok()
        .content_type("text/plain")
        .body(format!("{}", id.0.to_hyphenated()))
}

/// A helper for creating Actix routes that are resilient against double-slashes
///
/// Write `normpath("api/store")` to create a route pattern that matches "/api/store/",
/// "api//store", "api//store////", etc.
pub fn normpath(route: &str) -> String {
    let mut pattern = String::new();
    for (i, segment) in route.trim_matches('/').split('/').enumerate() {
        // Apparently the leading slash needs to be explicit and cannot be part of a pattern
        pattern.push_str(&format!(
            "/{{multislash{i}:/*}}{segment}",
            i = i,
            segment = segment
        ));
    }

    if route.ends_with('/') {
        pattern.push_str("{trailing_slash:/+}");
    } else {
        pattern.push_str("{trailing_slash:/*}");
    }
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normpath() {
        assert_eq!(
            normpath("/api/store/"),
            "/{multislash0:/*}api/{multislash1:/*}store{trailing_slash:/+}"
        );
        assert_eq!(
            normpath("/api/store"),
            "/{multislash0:/*}api/{multislash1:/*}store{trailing_slash:/*}"
        );
    }

    #[test]
    fn test_minimal_empty_event() {
        let json = r#"{}"#;
        let minimal = minimal_event_from_json(json.as_ref()).unwrap();
        assert_eq!(
            minimal,
            MinimalEvent {
                id: None,
                ty: EventType::Default,
            }
        );
    }

    #[test]
    fn test_minimal_event_id() {
        let json = r#"{"event_id": "037af9ac1b49494bacd7ec5114f801d9"}"#;
        let minimal = minimal_event_from_json(json.as_ref()).unwrap();
        assert_eq!(
            minimal,
            MinimalEvent {
                id: Some("037af9ac1b49494bacd7ec5114f801d9".parse().unwrap()),
                ty: EventType::Default,
            }
        );
    }

    #[test]
    fn test_minimal_event_type() {
        let json = r#"{"type": "expectct"}"#;
        let minimal = minimal_event_from_json(json.as_ref()).unwrap();
        assert_eq!(
            minimal,
            MinimalEvent {
                id: None,
                ty: EventType::ExpectCt,
            }
        );
    }

    #[test]
    fn test_minimal_event_invalid_type() {
        let json = r#"{"type": "invalid"}"#;
        let minimal = minimal_event_from_json(json.as_ref()).unwrap();
        assert_eq!(
            minimal,
            MinimalEvent {
                id: None,
                ty: EventType::Default,
            }
        );
    }
}
