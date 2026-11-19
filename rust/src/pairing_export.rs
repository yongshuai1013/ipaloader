//! 0.6.9: pairing-record export normalization, kept in its own
//! module because it is a pure function of the record bytes —
//! host-testable without the JNI/Android surface of the crate
//! (the lib as a whole only compiles for Android targets: it
//! calls `rustls_platform_verifier::android`).

/// The ten keys a complete lockdown pairing record carries.
pub const PAIRING_REQUIRED_KEYS: [&str; 10] = [
    "WiFiMACAddress",
    "SystemBUID",
    "RootPrivateKey",
    "HostPrivateKey",
    "HostID",
    "RootCertificate",
    "UDID",
    "EscrowBag",
    "HostCertificate",
    "DeviceCertificate",
];

/// Normalize a stored pairing record into export bytes: parse,
/// backfill the UDID from the stored identity when the record
/// lacks one (SideStore's import rejects records without it),
/// assert all ten lockdown keys are present and non-empty, and
/// re-serialize as an XML plist.
pub fn build_pairing_export(record: &[u8], known_udid: &str) -> Result<Vec<u8>, String> {
    let value = plist::Value::from_reader(std::io::Cursor::new(record))
        .map_err(|_| "配對紀錄解析失敗：不是有效的 plist".to_string())?;
    let plist::Value::Dictionary(mut dict) = value else {
        return Err("配對紀錄解析失敗：內容不是配對記錄的格式".to_string());
    };
    let has_udid = dict
        .get("UDID")
        .and_then(|v| v.as_string())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_udid {
        let udid = known_udid.trim();
        if udid.is_empty() {
            return Err("配對紀錄缺少 UDID，且沒有已知的裝置 UDID 可補入".to_string());
        }
        dict.insert("UDID".to_string(), plist::Value::String(udid.to_string()));
    }
    let mut missing: Vec<&str> = Vec::new();
    for key in PAIRING_REQUIRED_KEYS {
        let present = match dict.get(key) {
            Some(plist::Value::String(s)) => !s.trim().is_empty(),
            Some(plist::Value::Data(d)) => !d.is_empty(),
            _ => false,
        };
        if !present {
            missing.push(key);
        }
    }
    if !missing.is_empty() {
        return Err(format!("配對紀錄缺少必要欄位：{}", missing.join(", ")));
    }
    let value = plist::Value::Dictionary(dict);
    let mut buf: Vec<u8> = Vec::new();
    value
        .to_writer_xml(&mut buf)
        .map_err(|e| format!("配對紀錄序列化失敗：{}", e))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::{build_pairing_export, PAIRING_REQUIRED_KEYS};

    const KNOWN_UDID: &str = "00008030-001A2B3C4D5E6F7A";

    /// A complete lockdown pairing record with dummy material,
    /// in the shape idevice's `pair()` + `serialize()` produces.
    fn fixture_record(with_udid: bool, omit: Option<&str>) -> Vec<u8> {
        let mut dict = plist::Dictionary::new();
        let data_keys = [
            "DeviceCertificate",
            "HostPrivateKey",
            "HostCertificate",
            "RootPrivateKey",
            "RootCertificate",
            "EscrowBag",
        ];
        for key in data_keys {
            if omit == Some(key) {
                continue;
            }
            dict.insert(
                key.to_string(),
                plist::Value::Data(format!("dummy-{key}").into_bytes()),
            );
        }
        let string_keys = [
            ("WiFiMACAddress", "AA:BB:CC:DD:EE:FF"),
            ("SystemBUID", "11111111-2222-3333-4444-555555555555"),
            ("HostID", "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"),
        ];
        for (key, value) in string_keys {
            if omit == Some(key) {
                continue;
            }
            dict.insert(key.to_string(), plist::Value::String(value.to_string()));
        }
        if with_udid && omit != Some("UDID") {
            dict.insert(
                "UDID".to_string(),
                plist::Value::String(KNOWN_UDID.to_string()),
            );
        }
        let value = plist::Value::Dictionary(dict);
        let mut buf = Vec::new();
        value.to_writer_xml(&mut buf).expect("fixture serializes");
        buf
    }

    fn parse(bytes: &[u8]) -> plist::Dictionary {
        let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
            .expect("export parses as plist");
        match value {
            plist::Value::Dictionary(d) => d,
            _ => panic!("export root is not a dictionary"),
        }
    }

    #[test]
    fn complete_record_exports_with_all_ten_keys() {
        let record = fixture_record(true, None);
        let out = build_pairing_export(&record, KNOWN_UDID).expect("export ok");
        let dict = parse(&out);
        for key in PAIRING_REQUIRED_KEYS {
            assert!(dict.contains_key(key), "missing key {key}");
        }
        assert_eq!(
            dict.get("UDID").and_then(|v| v.as_string()),
            Some(KNOWN_UDID)
        );
    }

    #[test]
    fn missing_udid_is_backfilled_from_known_udid() {
        let record = fixture_record(false, None);
        let out = build_pairing_export(&record, KNOWN_UDID).expect("export ok");
        let dict = parse(&out);
        for key in PAIRING_REQUIRED_KEYS {
            assert!(dict.contains_key(key), "missing key {key}");
        }
        assert_eq!(
            dict.get("UDID").and_then(|v| v.as_string()),
            Some(KNOWN_UDID)
        );
    }

    #[test]
    fn missing_escrow_bag_is_rejected() {
        let record = fixture_record(true, Some("EscrowBag"));
        let err = build_pairing_export(&record, KNOWN_UDID).expect_err("must reject");
        assert!(err.contains("EscrowBag"), "error names the key: {err}");
    }

    #[test]
    fn garbage_input_is_rejected() {
        assert!(build_pairing_export(b"not a plist", KNOWN_UDID).is_err());
    }
}
