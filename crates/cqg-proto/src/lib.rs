//! CQG WebAPI protocol types, generated at build time from the pinned .proto files.
//!
//! Message names follow the .proto packages, e.g. `web_api_2::ClientMsg`,
//! `market_data_2::RealTimeMarketData`. Enum-typed fields are `u32` in the protocol
//! (CQG uses `uint32` + a documented enum), so compare against `Enum::Variant as u32`.

#![allow(clippy::all, clippy::pedantic, non_camel_case_types, rustdoc::all)]

include!(concat!(env!("OUT_DIR"), "/cqg_webapi.rs"));

/// Protocol version these types were generated from. Sent in `Logon` and stored in
/// recordings so a replay can tell which decoder it needs.
pub const PROTOCOL_VERSION_MAJOR: u32 =
    web_api_2::ProtocolVersionMajor::ProtocolVersionMajor as u32;
pub const PROTOCOL_VERSION_MINOR: u32 =
    web_api_2::ProtocolVersionMinor::ProtocolVersionMinor as u32;

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn protocol_version_is_2_x() {
        assert_eq!(PROTOCOL_VERSION_MAJOR, 2);
        assert!(PROTOCOL_VERSION_MINOR >= 230);
    }

    #[test]
    fn client_msg_roundtrip() {
        let msg = web_api_2::ClientMsg {
            logon: Some(user_session_2::Logon {
                client_version: "lines-test".into(),
                protocol_version_major: Some(PROTOCOL_VERSION_MAJOR),
                protocol_version_minor: Some(PROTOCOL_VERSION_MINOR),
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = msg.encode_to_vec();
        let back = web_api_2::ClientMsg::decode(bytes.as_slice()).unwrap();
        assert_eq!(back, msg);
    }
}
