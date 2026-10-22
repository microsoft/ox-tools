// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

struct FeatureArgs {
    all_features: bool,
    no_default_features: bool,
    features: Vec<String>,
}

enum MetadataFeatureOption {
    All,
    NoDefault,
    Some(Vec<String>),
}

fn metadata_feature_options(features: &FeatureArgs) -> Vec<MetadataFeatureOption> {
    let mut options = Vec::new();

    if features.all_features {
        options.push(MetadataFeatureOption::All);
    }

    if features.no_default_features {
        options.push(MetadataFeatureOption::NoDefault);
    }

    if !features.features.is_empty() {
        // Cargo accepts a comma-separated list in one argument and repetition across several, so
        // the entries are split apart here and handed over as the flat list they denote.
        let named = features
            .features
            .iter()
            .flat_map(|entry| entry.split([',', ' ']))
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        options.push(MetadataFeatureOption::Some(named));
    }

    options
}

fn standard_option() -> Option<u32> {
    Some(7)
}
