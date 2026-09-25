use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const SCHEMA_VERSION: u32 = 2;
pub(crate) const OSAKA_FORK: &str = "Osaka";
pub(crate) const EXECUTION_BOUNDARY: &str = "evm2_transaction_execution";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub schema_version: u32,
    pub workload_path: String,
    pub session_id: String,
    pub mode: Mode,
    pub samples: Vec<SampleSpec>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Diagnostic,
    Performance,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SampleSpec {
    pub sample_id: String,
    pub case_id: String,
    pub repetition: u64,
    pub phase: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workload {
    pub schema_version: u32,
    pub fork: String,
    pub generator: Generator,
    pub cases: Vec<WorkloadCase>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Generator {
    pub revision: String,
    pub seed: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkloadCase {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub target_operation: Option<String>,
    #[serde(default)]
    pub parameters: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub pre: Option<BTreeMap<String, Account>>,
    #[serde(default)]
    pub transactions: Option<Vec<Transaction>>,
    #[serde(default)]
    pub expected: Option<ExpectedOutcomes>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Account {
    pub balance: String,
    pub nonce: String,
    pub code: String,
    #[serde(default)]
    pub storage: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transaction {
    pub sender: String,
    /// Signing key for block-level workers that sign real transactions. evm2
    /// executes recovered intent, so it accepts and ignores this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    pub to: String,
    pub data: String,
    pub value: String,
    pub gas_limit: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedOutcomes {
    pub receipts: Vec<ExpectedReceipt>,
    #[serde(default)]
    pub storage: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedReceipt {
    pub success: bool,
    #[serde(default)]
    pub logs: Vec<ExpectedLog>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedLog {
    pub address: String,
    #[serde(default)]
    pub topics: Vec<String>,
    pub data: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResultRecord {
    pub schema_version: u32,
    pub session_id: String,
    pub sample_id: String,
    pub case_id: String,
    pub repetition: u64,
    pub phase: String,
    pub status: &'static str,
    pub execution_duration_ns: Option<u64>,
    pub execution_boundary: &'static str,
    pub baseline_hash: Option<String>,
    pub prepared_hash: Option<String>,
    pub commitment_hash: Option<String>,
    pub correctness_passed: bool,
    pub target_count: Option<u64>,
    pub opcode_counts: Option<BTreeMap<String, u64>>,
    pub declared_gas: Option<u64>,
    pub charged_gas: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResultError>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResultError {
    pub stage: &'static str,
    pub message: String,
}
