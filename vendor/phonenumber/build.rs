#[cfg(not(test))]
use std::env;
#[cfg(not(test))]
use std::fs::File;
#[cfg(not(test))]
use std::io::{BufReader, BufWriter};
#[cfg(not(test))]
use std::path::Path;

#[cfg(not(test))]
#[rustfmt::skip]
#[path = "src/metadata/loader.rs"]
mod loader;

#[cfg(not(test))]
#[rustfmt::skip]
#[path = "src/error.rs"]
mod error;

#[cfg(not(test))]
fn main() {
    let pnm_path = "assets/PhoneNumberMetadata.xml";
    let metadata = loader::load(BufReader::new(
        File::open(pnm_path).expect("could not open metadata file"),
    ))
    .expect("failed to load metadata");
    validate_patterns(&metadata);
    println!("cargo:rerun-if-changed={pnm_path}");

    let mut out = BufWriter::new(
        File::create(Path::new(&env::var("OUT_DIR").unwrap()).join("database.bin"))
            .expect("could not create database file"),
    );

    postcard::to_io(&metadata, &mut out).expect("failed to serialize database");
}

#[cfg(test)]
use phonenumber::metadata::loader;

// Validate the immutable bundled database once at build time. Database::from
// retains runtime validation for caller-provided metadata.
fn validate_patterns(metadata: &[loader::Metadata]) {
    let validate = |pattern: &str| {
        regex::RegexBuilder::new(pattern)
            .ignore_whitespace(true)
            .build()
            .expect("invalid bundled phone metadata regex");
    };
    for meta in metadata {
        for descriptor in [
            &meta.general,
            &meta.fixed_line,
            &meta.mobile,
            &meta.toll_free,
            &meta.premium_rate,
            &meta.shared_cost,
            &meta.personal_number,
            &meta.voip,
            &meta.pager,
            &meta.uan,
            &meta.emergency,
            &meta.voicemail,
            &meta.short_code,
            &meta.standard_rate,
            &meta.carrier,
            &meta.no_international,
        ]
        .into_iter()
        .flatten()
        {
            validate(
                descriptor
                    .national_number
                    .as_deref()
                    .expect("missing national number pattern"),
            );
        }
        for pattern in [
            &meta.international_prefix,
            &meta.national_prefix_for_parsing,
            &meta.leading_digits,
        ]
        .into_iter()
        .flatten()
        {
            validate(pattern);
        }
        for format in meta.formats.iter().chain(&meta.international_formats) {
            validate(format.pattern.as_deref().expect("missing format pattern"));
            for pattern in &format.leading_digits {
                validate(pattern);
            }
        }
    }
}

// Compiled by oxidezap-core's phone_metadata_build integration-test target.
// Reuse the public loader types; no copy of the production visitor is tested.
#[cfg(test)]
mod tests {
    use super::*;
    use phonenumber::metadata::Database;
    use std::io::Cursor;
    use std::panic::catch_unwind;

    fn valid_metadata() -> loader::Metadata {
        let descriptor = || {
            Some(loader::Descriptor {
                national_number: Some("[0-9]{7}".into()),
                ..Default::default()
            })
        };
        let format = || loader::Format {
            pattern: Some("([0-9]{3})([0-9]{4})".into()),
            format: Some("$1 $2".into()),
            leading_digits: vec!["[0-9]".into(), "[0-9]{2}".into()],
            ..Default::default()
        };
        loader::Metadata {
            id: Some("ZZ".into()),
            country_code: Some(999),
            general: descriptor(),
            fixed_line: descriptor(),
            mobile: descriptor(),
            toll_free: descriptor(),
            premium_rate: descriptor(),
            shared_cost: descriptor(),
            personal_number: descriptor(),
            voip: descriptor(),
            pager: descriptor(),
            uan: descriptor(),
            emergency: descriptor(),
            voicemail: descriptor(),
            short_code: descriptor(),
            standard_rate: descriptor(),
            carrier: descriptor(),
            no_international: descriptor(),
            international_prefix: Some("00".into()),
            national_prefix_for_parsing: Some("0".into()),
            leading_digits: Some("[0-9]".into()),
            formats: vec![format()],
            international_formats: vec![format()],
            ..Default::default()
        }
    }

    fn assert_rejected(metadata: loader::Metadata, category: &str) {
        // Ensure this fixture really reaches a checked runtime regex field.
        assert!(
            Database::from(vec![metadata.clone()]).is_err(),
            "{category}: checked path accepted invalid regex"
        );
        assert!(
            catch_unwind(|| validate_patterns(&[metadata])).is_err(),
            "{category}: build visitor omitted invalid regex"
        );
    }

    #[test]
    fn every_runtime_regex_category_is_validated_during_build() {
        let valid = valid_metadata();
        assert!(Database::from(vec![valid.clone()]).is_ok());
        validate_patterns(std::slice::from_ref(&valid));

        macro_rules! descriptors {
            ($($field:ident),+ $(,)?) => {$(
                let mut bad = valid.clone();
                bad.$field.as_mut().unwrap().national_number = Some("[".into());
                assert_rejected(bad, stringify!($field));
            )+};
        }
        descriptors!(
            general,
            fixed_line,
            mobile,
            toll_free,
            premium_rate,
            shared_cost,
            personal_number,
            voip,
            pager,
            uan,
            emergency,
            voicemail,
            short_code,
            standard_rate,
            carrier,
            no_international
        );

        macro_rules! optional_patterns {
            ($($field:ident),+ $(,)?) => {$(
                let mut bad = valid.clone();
                bad.$field = Some("[".into());
                assert_rejected(bad, stringify!($field));
            )+};
        }
        optional_patterns!(
            international_prefix,
            national_prefix_for_parsing,
            leading_digits
        );

        for international in [false, true] {
            let mut bad = valid.clone();
            let formats = if international {
                &mut bad.international_formats
            } else {
                &mut bad.formats
            };
            formats[0].pattern = Some("[".into());
            assert_rejected(
                bad,
                if international {
                    "international format"
                } else {
                    "national format"
                },
            );
            // Test every leading-digit entry, not only the final one used by
            // the formatter when selecting a rule.
            for index in 0..2 {
                let mut bad = valid.clone();
                let formats = if international {
                    &mut bad.international_formats
                } else {
                    &mut bad.formats
                };
                formats[0].leading_digits[index] = "[".into();
                assert_rejected(
                    bad,
                    if international {
                        "international leading digits"
                    } else {
                        "national leading digits"
                    },
                );
            }
        }
    }

    #[test]
    fn bundled_xml_passes_build_and_original_checked_runtime_paths() {
        let metadata =
            loader::load(Cursor::new(include_str!("assets/PhoneNumberMetadata.xml"))).unwrap();
        assert!(metadata.len() > 200);
        validate_patterns(&metadata);
        assert!(Database::from(metadata).is_ok());
    }
}
