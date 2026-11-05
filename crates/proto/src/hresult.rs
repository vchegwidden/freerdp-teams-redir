//! HRESULTs seen in the Windows add-in's replies (`msrdc-reply-structures.md` §1.5) [O]. On the wire
//! they are signed decimal numbers; `envelope` does the conversion.

pub const E_ABORT: u32 = 0x8000_4004;
pub const E_NOTIMPL: u32 = 0x8000_4001;
pub const E_UNEXPECTED: u32 = 0x8000_FFFF;
pub const E_INVALIDARG: u32 = 0x8007_0057;
pub const ERROR_ALREADY_EXISTS: u32 = 0x8007_00B7;
pub const ERROR_NOT_FOUND: u32 = 0x8007_0490;
pub const E_FAIL: u32 = 0x8000_4005;
/// An unknown object type or method: what the shipped client answers, with no `errorDetails`.
pub const ERROR_NOT_SUPPORTED: u32 = 0x8007_0032;
/// `SDP parse failure - line=[..] - ..` (`msrdc-reply-structures.md` §3c).
pub const ERROR_INVALID_DATA: u32 = 0x8007_000D;
