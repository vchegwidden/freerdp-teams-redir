//! Wire formats for the two Teams DVCs: `com.microsoft.rdc.dvc.webrtc.1` (JSON RPC)
//! and `com.microsoft.rdc.dvc.cpc.1` (SlimCore hello). No I/O and no media here.
//!
//! Labels follow the findings files: [O] observed, [I] inferred, [U] unknown.

pub mod cpc;
pub mod envelope;
pub mod errc;
pub mod frame;
pub mod hresult;
pub mod patch;

pub const DVC_WEBRTC: &str = "com.microsoft.rdc.dvc.webrtc.1";
pub const DVC_CPC: &str = "com.microsoft.rdc.dvc.cpc.1";
