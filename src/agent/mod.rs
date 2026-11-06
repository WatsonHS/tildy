//! Agent status: records in the OSC 7501 (Program Status Protocol rev 0.4)
//! vocabulary, their on-disk store and lifetime rules. Owner: M2 task B;
//! `osc` (the incremental OSC 7501 stream parser used by `tildy agent wrap`)
//! is task D's.
//!
//! Public interface (shared by `tildy agent publish|list|clear`, the
//! `agent_status` provider, the worker and `tildy agent wrap`):
//!
//! * [`Record`]: one status record (schema/agent-state.schema.json v1);
//!   [`Record::new`], [`Record::validate`], [`Record::normalize`].
//! * [`Store`]: [`Store::open`] / [`Store::open_with`],
//!   [`Store::publish`]`(Record) -> Result<`[`Stored`]`, `[`Reject`]`>`,
//!   [`Store::list`]`(&`[`Filter`]`)`, [`Store::clear`]`(&Filter)`,
//!   [`Store::drop_on_prompt`]`(origin_session)`, [`Store::expire`]`(now_ms)`,
//!   [`Store::next_expiry`]`()`, [`Store::scope_dir`]`(&Scope)`.
//! * [`parse_osc_body`]: a raw 7501 report body to a [`Report`]
//!   ([`Op::Set`] or [`Op::Clear`]), with the protocol's leniency rules.
//!
//! See docs/agent-status.md and docs/decisions/0008-agent-status.md.

pub mod osc;
pub mod record;
pub mod store;

pub use record::{
    base64_decode, base64_encode, check_app, check_id, id_within, parse_osc_body, strip_controls, truncate_bytes, Kind, Op, Record,
    Reject, Report, Scope, ScopeKind, Source, State, RECORD_VERSION,
};
pub use store::{now_ms, Filter, Store, Stored};
