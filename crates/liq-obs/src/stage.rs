//! `stage()` subscriber: histograms + thread-level `ConfigVersion` (00C carry-forward).
//!
//! Hot path calls `liq_types::stage(trace, stage)` and never links this crate.
//! `liq_config::ConfigVersion` cannot be a type here (D46: no `liq-config` dep).
//! Install the 32-byte hash on the telemetry thread; the layer copies it onto
//! every `stage` event as a field.

use std::cell::Cell;

use alloy_primitives::B256;
use liq_types::Stage;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

thread_local! {
    static CONFIG_VERSION: Cell<Option<B256>> = const { Cell::new(None) };
}

/// Bind this OS thread's spans to a `ConfigVersion` hash. Call once after
/// config load on the telemetry / engine-join thread.
pub fn install_config_version(hash: B256) {
    CONFIG_VERSION.with(|c| c.set(Some(hash)));
}

#[must_use]
pub fn current_config_version() -> Option<B256> {
    CONFIG_VERSION.with(Cell::get)
}

/// Layer that records `trace`, `stage`, and `config_version` on `liq_types::stage` events.
#[derive(Clone, Debug, Default)]
pub struct StageLayer;

struct StageVisit {
    trace: Option<u64>,
    stage: Option<String>,
}

impl Visit for StageVisit {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "trace" {
            self.trace = Some(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "stage" {
            self.stage = Some(format!("{value:?}"));
        }
    }
}

fn record_stage(stage_dbg: &str) {
    let name = match stage_dbg {
        "PriceTick" => "price_tick",
        "Candidate" => "candidate",
        "Quote" => "quote",
        "RouteSolved" => "route_solved",
        "SimVerified" => "sim_verified",
        "Signed" => "signed",
        "VenueAck" => "venue_ack",
        "Inclusion" => "inclusion",
        other => {
            tracing::error!(other, "unknown Stage discriminant on emit");
            return;
        }
    };
    metrics::counter!("liq_stage_events", "stage" => name).increment(1);
}

impl<S: Subscriber> Layer<S> for StageLayer {
    fn enabled(&self, meta: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        meta.fields().field("stage").is_some() && meta.fields().field("trace").is_some()
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut v = StageVisit {
            trace: None,
            stage: None,
        };
        event.record(&mut v);
        let Some(trace) = v.trace else {
            return;
        };
        let Some(stage) = v.stage else {
            return;
        };
        let cv = current_config_version();
        match cv {
            Some(hash) => {
                tracing::debug!(
                    trace,
                    stage = stage.as_str(),
                    config_version = %hash,
                    "stage boundary"
                );
            }
            None => {
                tracing::error!(
                    trace,
                    stage = stage.as_str(),
                    "ConfigVersion not installed on this thread; stage span is not reproducible"
                );
            }
        }
        record_stage(stage.trim_matches('"'));
    }

    fn on_new_span(&self, _attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {}

    fn on_record(&self, _span: &Id, _values: &Record<'_>, _ctx: Context<'_, S>) {}
}

/// All GUIDE 09 §1 boundaries. Used by tests to assert the subscriber sees each.
#[must_use]
pub fn all_stages() -> [Stage; 8] {
    [
        Stage::PriceTick,
        Stage::Candidate,
        Stage::Quote,
        Stage::RouteSolved,
        Stage::SimVerified,
        Stage::Signed,
        Stage::VenueAck,
        Stage::Inclusion,
    ]
}
