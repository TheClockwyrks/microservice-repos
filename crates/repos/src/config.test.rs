//! The configuration's resolution from the environment.

use std::collections::HashMap;

use super::*;

const AZURITE_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

fn resolve(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    Config::from_lookup(|key| vars.get(key).cloned())
}

const CONTAINER: [(&str, &str); 2] = [
    ("TCAB_REPOS_BLOB_ACCOUNT", "tcabstagingstore"),
    ("TCAB_REPOS_BLOB_CONTAINER", "bundles"),
];

#[test]
fn the_defaults_bind_all_interfaces_and_use_workload_identity() {
    let config = resolve(&CONTAINER).unwrap();
    assert_eq!(config.bind, "0.0.0.0:8792");
    assert_eq!(config.backend_url, "http://127.0.0.1:8787");
    assert_eq!(config.service_token, None);
    assert_eq!(config.blob.account, "tcabstagingstore");
    assert_eq!(config.blob.container, "bundles");
    assert_eq!(
        config.blob.endpoint,
        "https://tcabstagingstore.blob.core.windows.net"
    );
    assert_eq!(config.blob.auth, BlobAuth::WorkloadIdentity);
    assert!(!config.creates_container());
    assert!(config.github.is_none());
}

#[test]
fn every_variable_is_read() {
    let mut vars = CONTAINER.to_vec();
    vars.extend([
        ("TCAB_REPOS_BIND", "127.0.0.1:9000"),
        ("TCAB_BACKEND_URL", "http://tcab-backend:8787/"),
        ("TCAB_BACKEND_SERVICE_TOKEN", "svc"),
        (
            "TCAB_REPOS_BLOB_ENDPOINT",
            "http://tcab-azurite:10000/devstoreaccount1/",
        ),
        ("TCAB_REPOS_BLOB_KEY", AZURITE_KEY),
        ("TCAB_GITHUB_ORG", "TheClockwyrks-Staging"),
        ("TCAB_GITHUB_TOKEN", "ghp_secret"),
    ]);
    let config = resolve(&vars).unwrap();
    assert_eq!(config.bind, "127.0.0.1:9000");
    assert_eq!(config.backend_url, "http://tcab-backend:8787");
    assert_eq!(config.service_token.as_deref(), Some("svc"));
    assert_eq!(
        config.blob.endpoint,
        "http://tcab-azurite:10000/devstoreaccount1"
    );
    assert!(matches!(config.blob.auth, BlobAuth::SharedKey(ref key) if key.len() == 64));
    assert!(config.creates_container());
    let github = config.github.unwrap();
    assert_eq!(github.org, "TheClockwyrks-Staging");
    assert_eq!(github.token, "ghp_secret");
    assert_eq!(
        github.repo_url("tcab-x"),
        "https://github.com/TheClockwyrks-Staging/tcab-x"
    );
    assert!(!format!("{github:?}").contains("ghp_secret"));
}

#[test]
fn the_container_is_required() {
    assert!(matches!(
        resolve(&[("TCAB_REPOS_BLOB_CONTAINER", "bundles")]),
        Err(ConfigError::Missing("TCAB_REPOS_BLOB_ACCOUNT"))
    ));
    assert!(matches!(
        resolve(&[
            ("TCAB_REPOS_BLOB_ACCOUNT", "a"),
            ("TCAB_REPOS_BLOB_CONTAINER", " ")
        ]),
        Err(ConfigError::Missing("TCAB_REPOS_BLOB_CONTAINER"))
    ));
}

#[test]
fn the_emulator_pair_is_set_together() {
    let mut vars = CONTAINER.to_vec();
    vars.push((
        "TCAB_REPOS_BLOB_ENDPOINT",
        "http://azurite:10000/devstoreaccount1",
    ));
    assert!(matches!(
        resolve(&vars),
        Err(ConfigError::Unpaired {
            set: "TCAB_REPOS_BLOB_ENDPOINT",
            ..
        })
    ));
    let mut vars = CONTAINER.to_vec();
    vars.push(("TCAB_REPOS_BLOB_KEY", AZURITE_KEY));
    assert!(matches!(
        resolve(&vars),
        Err(ConfigError::Unpaired {
            set: "TCAB_REPOS_BLOB_KEY",
            ..
        })
    ));
}

#[test]
fn a_key_that_is_not_base64_is_refused() {
    let mut vars = CONTAINER.to_vec();
    vars.extend([
        (
            "TCAB_REPOS_BLOB_ENDPOINT",
            "http://azurite:10000/devstoreaccount1",
        ),
        ("TCAB_REPOS_BLOB_KEY", "not base64!"),
    ]);
    assert!(matches!(resolve(&vars), Err(ConfigError::InvalidBlobKey)));
}

#[test]
fn the_github_pair_is_set_together() {
    let mut vars = CONTAINER.to_vec();
    vars.push(("TCAB_GITHUB_ORG", "TheClockwyrks"));
    assert!(matches!(
        resolve(&vars),
        Err(ConfigError::Unpaired {
            set: "TCAB_GITHUB_ORG",
            unset: "TCAB_GITHUB_TOKEN",
            ..
        })
    ));
    let mut vars = CONTAINER.to_vec();
    vars.push(("TCAB_GITHUB_TOKEN", "t"));
    assert!(matches!(
        resolve(&vars),
        Err(ConfigError::Unpaired {
            set: "TCAB_GITHUB_TOKEN",
            ..
        })
    ));
}
