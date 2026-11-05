//! `vdi::plugin::errc` values and their wire encoding.
//!
//! `vdi::EncodeErrorCode` (libMsTeamsPluginAvd 0x89f4) packs `(value << 3) | category`, where
//! generic = 0, system = 1 and the plugin category (`&__MergedGlobals`, the one
//! `make_error_code(errc)` returns) = 2 [O]. The Windows and MSIX categories take 3 and 4;
//! which is which was not traced, and they are not used here.

pub const NO_PLUGIN: i32 = 2000;
pub const CHANNEL_NOT_AVAILABLE: i32 = 0x7d1;
pub const VM_VERSION_BLOCKED: i32 = 0x7d2;
pub const VIRTUAL_CHANNEL_DISALLOWED: i32 = 0x7d3;
pub const CWA_VERSION_BLOCKED: i32 = 0x7d4;
pub const SEAMLESS_MODE_NOT_SUPPORTED: i32 = 0x7d5;
pub const SHARING_PROTECTION_NOT_SUPPORTED: i32 = 0x7d6;
pub const CLIENT_VERSION_NOT_SUPPORTED: i32 = 0x7d7;
pub const CLIENT_APP_STORE_BUILD: i32 = 0x7d8;

const CATEGORY_PLUGIN: i32 = 2;

/// Encodes a plugin `errc` the way `ClientHello`'s `load_errc` / `install_errc` carry it.
pub fn encode_plugin(code: i32) -> i32 {
    (code << 3) | CATEGORY_PLUGIN
}

/// Reverse of [`encode_plugin`]; `None` if the category is not the plugin category.
pub fn decode_plugin(encoded: i32) -> Option<i32> {
    ((encoded & 7) == CATEGORY_PLUGIN).then_some(encoded >> 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_the_decompiled_shift_and_category() {
        assert_eq!(encode_plugin(NO_PLUGIN), 16002);
        assert_eq!(encode_plugin(CLIENT_APP_STORE_BUILD), 16066);
        assert_eq!(decode_plugin(16066), Some(CLIENT_APP_STORE_BUILD));
        assert_eq!(decode_plugin(16065), None);
    }
}
