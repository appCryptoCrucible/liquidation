//! Cross-cutting ids, fixed-point, price, halt, subscribe, submit, and trace types.

pub mod fixed;
pub use fixed::{Ray, RayU128, Wad};

pub mod band;
pub mod halt;
pub mod ids;
pub mod price;
pub mod submit;
pub mod subscribe;
pub mod trace;

pub use band::Band;
pub use halt::{HaltReason, HaltScope, HaltSink, TriggerKind};
pub use ids::{AssetId, ChainId, FlashProvider, MarketId, PositionId, PositionKey, ProtocolId};
pub use price::{
    Confidence, MevShareHint, Price, PriceTick, PriceVector, ScheduledParamChange, SourceKind,
};
pub use submit::{BuilderId, IntendedSubmission, SubmitReceipt, Submitter, Venue};
pub use subscribe::{LogFilter, LogSubscriber};
pub use trace::{stage, Stage, TraceId};
