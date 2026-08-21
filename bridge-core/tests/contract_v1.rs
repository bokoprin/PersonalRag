use std::{fs, path::PathBuf};

use personalrag_gui_bridge_core::{
    BackgroundStatus, ContractInfo, IndexRequest, SearchHit, SearchRequest, SnippetHit,
    SnippetRequest, APP_CONTRACT_NAME, APP_CONTRACT_VERSION,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

fn contract_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../app-contract/v1")
}

fn fixture_value(name: &str) -> Value {
    let bytes = fs::read(contract_root().join("fixtures").join(name)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn fixture<T: DeserializeOwned>(name: &str) -> T {
    serde_json::from_value(fixture_value(name)).unwrap()
}

fn assert_fixture_round_trip<T>(name: &str)
where
    T: DeserializeOwned + Serialize,
{
    let expected = fixture_value(name);
    let parsed: T = serde_json::from_value(expected.clone()).unwrap();
    let actual = serde_json::to_value(parsed).unwrap();
    assert_eq!(actual, expected, "fixture drift: {name}");
}

#[test]
fn app_contract_v1_fixtures_round_trip_in_rust() {
    assert_fixture_round_trip::<ContractInfo>("contract-info.json");
    assert_fixture_round_trip::<SearchRequest>("search-request.json");
    assert_fixture_round_trip::<SearchHit>("search-hit.json");
    assert_fixture_round_trip::<IndexRequest>("index-request.json");
    assert_fixture_round_trip::<SnippetRequest>("snippet-request.json");
    assert_fixture_round_trip::<SnippetHit>("snippet-hit.json");
    assert_fixture_round_trip::<BackgroundStatus>("background-status.json");
}

#[test]
fn app_contract_v1_manifest_and_runtime_version_match() {
    let contract: Value =
        serde_json::from_slice(&fs::read(contract_root().join("contract.json")).unwrap()).unwrap();
    assert_eq!(contract["name"], APP_CONTRACT_NAME);
    assert_eq!(contract["version"], APP_CONTRACT_VERSION);
    let info: ContractInfo = fixture("contract-info.json");
    assert_eq!(info, ContractInfo::default());
}

#[test]
fn app_contract_v1_rejects_unannounced_request_fields() {
    let mut value = fixture_value("search-request.json");
    value
        .as_object_mut()
        .unwrap()
        .insert("future_field".to_owned(), Value::Bool(true));
    assert!(serde_json::from_value::<SearchRequest>(value).is_err());
}
