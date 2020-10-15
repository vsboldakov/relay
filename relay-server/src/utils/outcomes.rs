use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use actix::prelude::*;
use parking_lot::Mutex;

use relay_common::{metric, DataCategory};
use relay_general::protocol::EventId;
use relay_quotas::Scoping;

use crate::actors::outcome::{DiscardReason, Outcome, OutcomeProducer, TrackOutcome};
use crate::envelope::Envelope;
use crate::extractors::RequestMeta;
use crate::metrics::RelayCounters;
use crate::utils::EnvelopeSummary;

#[derive(Clone)]
struct OutcomeHandlerInner {
    producer: Addr<OutcomeProducer>,
    scoping: Scoping,
    start_time: Instant,
    event_id: Option<EventId>,
    remote_addr: Option<IpAddr>,
    summary: EnvelopeSummary,
}

impl OutcomeHandlerInner {
    fn reset(&mut self) {
        self.summary = EnvelopeSummary::empty();
    }

    fn track_one(&self, outcome: Outcome, _category: DataCategory, _quantity: usize) {
        self.producer.do_send(TrackOutcome {
            timestamp: self.start_time,
            scoping: self.scoping.clone(),
            event_id: self.event_id,
            remote_addr: self.remote_addr,
            outcome,
            // category,
            // quantity, // TODO: .max(1) ? Check with sentry
        });

        // TODO: Reduce quantity here? -> we could rely on rescope.
    }

    fn track_remaining(&mut self, outcome: Outcome) {
        if let Some(category) = self.summary.event_category {
            self.track_one(outcome.clone(), category, 1);
        }

        let attachment_quantity = self.summary.attachment_quantity;
        if attachment_quantity > 0 {
            self.track_one(
                outcome.clone(),
                DataCategory::Attachment,
                attachment_quantity,
            );
        }

        let session_quantity = self.summary.session_quantity;
        if session_quantity > 0 {
            self.track_one(outcome.clone(), DataCategory::Session, session_quantity);
        }
    }
}

impl Drop for OutcomeHandlerInner {
    fn drop(&mut self) {
        // TODO: Track the metric, but only if the summary is non-empty
        self.track_remaining(Outcome::Invalid(DiscardReason::Internal));
    }
}

impl fmt::Debug for OutcomeHandlerInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutcomeHandlerInner")
            .field("scoping", &self.scoping)
            .field("start_time", &self.start_time)
            .field("event_id", &self.event_id)
            .field("remote_addr", &self.remote_addr)
            .field("summary", &self.summary)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct OutcomeHandler {
    inner: Arc<Mutex<OutcomeHandlerInner>>,
}

impl<'a> OutcomeHandler {
    pub fn new(producer: Addr<OutcomeProducer>, request: &RequestMeta) -> Self {
        let inner = Arc::new(Mutex::new(OutcomeHandlerInner {
            producer,
            scoping: request.get_partial_scoping(),
            start_time: request.start_time(),
            event_id: None,
            remote_addr: request.client_addr(),
            summary: EnvelopeSummary::empty(),
        }));

        Self { inner }
    }

    pub fn split(&self) -> Self {
        let inner = Arc::new(Mutex::new(self.inner.lock().clone()));
        Self { inner }
    }

    pub fn update_scoping(&self, scoping: Scoping) {
        self.inner.lock().scoping = scoping;
    }

    pub fn update(&self, envelope: &Envelope) {
        self.inner.lock().summary = EnvelopeSummary::compute(envelope);
    }

    pub fn reject_one(&self, outcome: Outcome, category: DataCategory, quantity: usize) {
        self.inner.lock().track_one(outcome, category, quantity)
    }

    /// May be called multiple times, only counts once.
    pub fn reject(self, outcome: Outcome) {
        // TODO: This must be logged in the drop case, too. Also: what if the envelope is empty
        // after `track_one`? Probably same...
        metric!(counter(RelayCounters::EnvelopeRejected) += 1);

        let mut inner = self.inner.lock();
        inner.track_remaining(outcome);
        inner.reset();
    }

    pub fn accept(self) {
        metric!(counter(RelayCounters::EnvelopeAccepted) += 1);
        self.inner.lock().reset();
    }
}
