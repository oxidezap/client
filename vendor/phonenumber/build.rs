use std::env;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

#[path = "src/metadata/loader.rs"]
mod loader;

#[path = "src/error.rs"]
mod error;

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
