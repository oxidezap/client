use phonenumber::metadata::{Database, loader};

#[test]
fn custom_lists_survive_regex_eviction() {
    let patterns: Vec<_> = (1..=160).map(|n| format!("[0-9]{{{n}}}")).collect();
    let db = Database::from(vec![loader::Metadata {
        id: Some("ZZ".into()),
        country_code: Some(999),
        general: Some(loader::Descriptor {
            national_number: Some("[0-9]+".into()),
            possible_length: vec![7, 9, 11],
            possible_local_length: vec![4, 5],
            ..Default::default()
        }),
        formats: patterns
            .iter()
            .map(|pattern| loader::Format {
                pattern: Some(pattern.clone()),
                format: Some("$0".into()),
                leading_digits: vec!["[0-9]".into(), "[0-9]{2}".into()],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }])
    .unwrap();
    let metadata = db.by_id("ZZ").unwrap();
    assert_eq!(
        metadata.descriptors().general().possible_length(),
        &[7, 9, 11]
    );
    assert_eq!(
        metadata.descriptors().general().possible_local_length(),
        &[4, 5]
    );
    assert_eq!(metadata.formats().len(), patterns.len());
    for _ in 0..2 {
        for (index, format) in metadata.formats().iter().enumerate() {
            assert_eq!(format.pattern().as_str(), patterns[index]);
            assert!(format.pattern().is_match(&"0".repeat(index + 1)));
            assert_eq!(format.leading_digits().len(), 2);
        }
        assert_eq!(db.cache().lock().unwrap().len(), 100);
    }
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn formatting_is_consistent_across_threads() {
    let inputs = [
        "12025550123",
        "442079460000",
        "61491570156",
        "390600000000",
        "552120000000",
        "49300000000",
        "33100000000",
        "911100000000",
        "81300000000",
        "74950000000",
        "77000000000",
        "80010000000",
    ];
    let expected: Vec<_> = inputs
        .iter()
        .map(|digits| oxidezap_core::format_phone_jid(&format!("{digits}@s.whatsapp.net")))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..10 {
                    for (digits, expected) in inputs.iter().zip(&expected) {
                        assert_eq!(
                            &oxidezap_core::format_phone_jid(&format!("{digits}@s.whatsapp.net")),
                            expected
                        );
                    }
                }
            });
        }
    });
}
