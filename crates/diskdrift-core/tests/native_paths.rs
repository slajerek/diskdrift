use diskdrift_core::native::{NativeName, NativePath, NativeRoot, PathEncoding, RootKind};

#[test]
fn native_names_round_trip_losslessly() {
    let unix = NativeName::unix(vec![b'a', 0xff, b'z']);
    let unix_json = serde_json::to_value(&unix).unwrap();
    assert_eq!(unix_json["pathEncoding"], "unix_bytes");
    assert_eq!(unix_json["pathBytes"], "Yf96");
    assert_eq!(
        serde_json::from_value::<NativeName>(unix_json).unwrap(),
        unix
    );

    let windows = NativeName::windows(vec![b'a' as u16, 0xd800, b'z' as u16]);
    let windows_json = serde_json::to_value(&windows).unwrap();
    assert_eq!(windows_json["pathEncoding"], "windows_utf16le");
    assert_eq!(
        serde_json::from_value::<NativeName>(windows_json).unwrap(),
        windows
    );
}

#[test]
fn canonical_order_uses_native_units() {
    assert!(NativeName::unix(vec![0x7f]) < NativeName::unix(vec![0x80]));
    assert!(NativeName::windows(vec![0x0100]) < NativeName::windows(vec![0x0200]));
    assert!(NativeName::windows(vec![0x0100]) < NativeName::windows(vec![0x01ff]));
}

#[test]
fn tagged_roots_match_all_spec_examples() {
    let examples = [
        (
            r#"{"path":"/","pathEncoding":"unix_bytes","rootKind":"posix"}"#,
            NativeRoot::posix(vec![b'/']),
        ),
        (
            r#"{"path":"/tmp/�","pathEncoding":"unix_bytes","pathBytes":"L3RtcC//","rootKind":"posix"}"#,
            NativeRoot::posix(b"/tmp/\xff".to_vec()),
        ),
        (
            r#"{"path":"C:\\","pathEncoding":"windows_utf16le","pathWide":"QwA6AFwA","rootKind":"drive","verbatim":false}"#,
            NativeRoot::new(
                NativePath::windows("C:\\".encode_utf16().collect()),
                RootKind::Drive,
                false,
            )
            .unwrap(),
        ),
        (
            r#"{"path":"\\\\server\\share\\","pathEncoding":"windows_utf16le","pathWide":"XABcAHMAZQByAHYAZQByAFwAcwBoAGEAcgBlAFwA","rootKind":"unc","verbatim":false}"#,
            NativeRoot::new(
                NativePath::windows("\\\\server\\share\\".encode_utf16().collect()),
                RootKind::Unc,
                false,
            )
            .unwrap(),
        ),
        (
            r#"{"path":"\\\\?\\Volume{01234567-89ab-cdef-0123-456789abcdef}\\","pathEncoding":"windows_utf16le","pathWide":"XABcAD8AXABWAG8AbAB1AG0AZQB7ADAAMQAyADMANAA1ADYANwAtADgAOQBhAGIALQBjAGQAZQBmAC0AMAAxADIAMwAtADQANQA2ADcAOAA5AGEAYgBjAGQAZQBmAH0AXAA=","rootKind":"volume_guid","verbatim":true}"#,
            NativeRoot::new(
                NativePath::windows(
                    "\\\\?\\Volume{01234567-89ab-cdef-0123-456789abcdef}\\"
                        .encode_utf16()
                        .collect(),
                ),
                RootKind::VolumeGuid,
                true,
            )
            .unwrap(),
        ),
        (
            r#"{"path":"\\\\?\\C:\\data","pathEncoding":"windows_utf16le","pathWide":"XABcAD8AXABDADoAXABkAGEAdABhAA==","rootKind":"drive","verbatim":true}"#,
            NativeRoot::new(
                NativePath::windows("\\\\?\\C:\\data".encode_utf16().collect()),
                RootKind::Drive,
                true,
            )
            .unwrap(),
        ),
        (
            r#"{"path":"C:\\�","pathEncoding":"windows_utf16le","pathWide":"QwA6AFwAANg=","rootKind":"drive","verbatim":false}"#,
            NativeRoot::new(
                NativePath::windows(vec![b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]),
                RootKind::Drive,
                false,
            )
            .unwrap(),
        ),
    ];

    for (json, root) in examples {
        assert_eq!(serde_json::to_string(&root).unwrap(), json);
        assert_eq!(serde_json::from_str::<NativeRoot>(json).unwrap(), root);
    }
}

#[test]
fn invalid_roots_are_rejected() {
    let invalid = [
        NativeRoot::new(
            NativePath::unix(b"relative".to_vec()),
            RootKind::Posix,
            false,
        ),
        NativeRoot::new(
            NativePath::unix(b"/tmp\0x".to_vec()),
            RootKind::Posix,
            false,
        ),
        NativeRoot::new(
            NativePath::windows("C:relative".encode_utf16().collect()),
            RootKind::Drive,
            false,
        ),
        NativeRoot::new(
            NativePath::windows("\\\\?\\C:\\data".encode_utf16().collect()),
            RootKind::Drive,
            false,
        ),
    ];
    assert!(invalid.into_iter().all(|value| value.is_err()));

    let mut display_disagrees =
        serde_json::to_value(NativeRoot::posix(b"/tmp/a".to_vec())).unwrap();
    display_disagrees["path"] = serde_json::Value::String("/tmp/b".into());
    display_disagrees["pathBytes"] = serde_json::Value::String("L3RtcC9h".into());
    assert!(serde_json::from_value::<NativeRoot>(display_disagrees).is_err());

    let _ = PathEncoding::UnixBytes;
}

#[test]
fn path_bytes_are_rejected_when_the_payload_is_valid_utf8() {
    let value = serde_json::json!({
        "path": "/tmp/ok",
        "pathEncoding": "unix_bytes",
        "pathBytes": "L3RtcC9vaw=="
    });
    assert!(serde_json::from_value::<NativePath>(value).is_err());
}

#[test]
fn windows_root_requires_an_explicit_verbatim_field() {
    let value = serde_json::json!({
        "path": "C:\\\\",
        "pathEncoding": "windows_utf16le",
        "pathWide": "QwA6AFwA",
        "rootKind": "drive"
    });
    assert!(serde_json::from_value::<NativeRoot>(value).is_err());
}

#[test]
fn windows_device_forms_are_rejected() {
    for path in [r"\\.\PhysicalDrive0\", r"\\?\GLOBALROOT\Device\Harddisk0\", r"\??\C:\"] {
        let root = NativeRoot::new(
            NativePath::windows(path.encode_utf16().collect()),
            RootKind::Unc,
            false,
        );
        assert!(root.is_err(), "accepted device path {path:?}");
    }
}

#[test]
fn tagged_native_schemas_reject_unknown_fields() {
    let mut path = serde_json::to_value(NativePath::unix(b"/tmp".to_vec())).unwrap();
    path["extra"] = true.into();
    assert!(serde_json::from_value::<NativePath>(path).is_err());

    let mut root = serde_json::to_value(NativeRoot::posix(b"/tmp".to_vec())).unwrap();
    root["extra"] = true.into();
    assert!(serde_json::from_value::<NativeRoot>(root).is_err());
}
