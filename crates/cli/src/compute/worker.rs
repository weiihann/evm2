use crate::protocol::{
    EXECUTION_BOUNDARY, ExpectedLog, ExpectedOutcomes, ExpectedReceipt, WorkloadCase,
};
use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use evm2::{
    BaseEvmTypes, Evm, Inspector, Precompiles, SpecId,
    bytecode::Bytecode,
    env::BlockEnv,
    ethereum::{RecoveredTxEnvelope, TxEnvelope, ethereum_tx_registry},
    evm::{AccountInfo, InMemoryDB},
    interpreter::{Interpreter, Message, MessageResult, opcode::OpCode},
    precompile::PrecompileProvider,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    str::FromStr,
    time::Instant,
};

const BACKEND_IDENTITY: &str = "evm2-osaka-interpreter";
type BenchEvm = Evm<'static, BaseEvmTypes>;

#[derive(Clone, Debug)]
pub(crate) struct PreparedCase {
    pub baseline: InMemoryDB,
    pub target_count_key: String,
    pub transactions: Vec<RecoveredTxEnvelope>,
    pub expected: ExpectedOutcomes,
    pub baseline_hash: String,
    pub prepared_hash: String,
    pub declared_gas: u64,
}

#[derive(Debug)]
pub(crate) struct ExecutionObservation {
    pub duration_ns: u64,
    pub commitment_hash: String,
    pub charged_gas: u64,
    pub opcode_counts: Option<BTreeMap<String, u64>>,
    pub target_count: Option<u64>,
    pub correctness_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct PreparedIdentity<'a> {
    fork: &'static str,
    backend: &'static str,
    execution_boundary: &'static str,
    case: &'a WorkloadCase,
}

#[derive(Debug, Serialize)]
struct AccountSnapshot {
    balance: String,
    nonce: u64,
    code_hash: String,
}

#[derive(Debug, Serialize)]
struct StateSnapshot {
    accounts: BTreeMap<String, Option<AccountSnapshot>>,
    storage: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Serialize)]
struct Commitment<'a> {
    receipts: &'a [ExpectedReceipt],
    state: StateSnapshot,
}

#[derive(Debug, Default)]
struct ComputeInspector {
    precompiles: HashSet<Address>,
    counts: BTreeMap<String, u64>,
}

impl ComputeInspector {
    fn new(precompiles: HashSet<Address>, target_count_key: &str) -> Self {
        let mut counts = BTreeMap::new();
        counts.insert(target_count_key.to_owned(), 0);
        Self { precompiles, counts }
    }
}

impl Inspector<BaseEvmTypes> for ComputeInspector {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, BaseEvmTypes>) {
        let Some(opcode) = OpCode::new(interp.opcode()) else {
            return;
        };
        let mut name = opcode.to_string();
        if name == "SHA3" {
            name = "KECCAK256".to_owned();
        }
        *self.counts.entry(name).or_default() += 1;
    }

    fn call(
        &mut self,
        _interp: &mut Interpreter<'_, '_, BaseEvmTypes>,
        message: &mut Message<BaseEvmTypes>,
    ) -> Option<MessageResult<BaseEvmTypes>> {
        if !message.disable_precompiles && self.precompiles.contains(&message.code_address) {
            let key = format!("PRECOMPILE_{:#x}", message.code_address);
            *self.counts.entry(key).or_default() += 1;
        }
        None
    }
}

pub(crate) fn prepare_case(case: &WorkloadCase) -> Result<PreparedCase, String> {
    if case.status != "ready" {
        return Err(case.reason.clone().unwrap_or_else(|| "case is not ready".to_owned()));
    }
    let target_operation = case
        .target_operation
        .clone()
        .filter(|target| !target.is_empty())
        .ok_or_else(|| "ready case has no target_operation".to_owned())?;
    let target_count_key = case
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.get("precompile_address"))
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || target_operation.clone(),
            |address| format!("PRECOMPILE_{}", address.to_ascii_lowercase()),
        );
    let pre = case.pre.as_ref().ok_or_else(|| "ready case has no prestate".to_owned())?;
    let transactions =
        case.transactions.as_ref().ok_or_else(|| "ready case has no transactions".to_owned())?;
    let expected =
        case.expected.clone().ok_or_else(|| "ready case has no expected outcomes".to_owned())?;
    if expected.receipts.len() != transactions.len() {
        return Err(format!(
            "case declares {} transactions but {} expected receipts",
            transactions.len(),
            expected.receipts.len()
        ));
    }

    let mut baseline = InMemoryDB::default();
    for (raw_address, account) in pre {
        let address = parse_address(raw_address)?;
        let mut info = AccountInfo::default()
            .with_balance(parse_u256(&account.balance)?)
            .with_nonce(parse_u64(&account.nonce)?);
        let code = parse_bytes(&account.code)?;
        if !code.is_empty() {
            info = info.with_code(Bytecode::new_legacy(code));
        }
        baseline.insert_account_info(&address, info);
        for (raw_slot, raw_value) in &account.storage {
            baseline.insert_account_storage(
                &address,
                &parse_u256(raw_slot)?,
                &parse_u256(raw_value)?,
            );
        }
    }

    let mut next_nonce = HashMap::<Address, u64>::new();
    let mut prepared_transactions = Vec::with_capacity(transactions.len());
    let mut declared_gas = 0_u64;
    for transaction in transactions {
        let sender = parse_address(&transaction.sender)?;
        let nonce = next_nonce
            .entry(sender)
            .or_insert_with(|| baseline.account_info(&sender).map_or(0, |account| account.nonce));
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: *nonce,
            gas_price: 0,
            gas_limit: transaction.gas_limit,
            to: TxKind::Call(parse_address(&transaction.to)?),
            value: parse_u256(&transaction.value)?,
            input: parse_bytes(&transaction.data)?,
        };
        *nonce = nonce
            .checked_add(1)
            .ok_or_else(|| format!("sender {} nonce overflows u64", transaction.sender))?;
        declared_gas = declared_gas
            .checked_add(transaction.gas_limit)
            .ok_or_else(|| "declared transaction gas sum overflows u64".to_owned())?;
        prepared_transactions.push(Recovered::new_unchecked(TxEnvelope::Legacy(tx), sender));
    }

    let baseline_hash = hash_json(&state_snapshot(&baseline))?;
    let prepared_hash = hash_json(&PreparedIdentity {
        fork: "Osaka",
        backend: BACKEND_IDENTITY,
        execution_boundary: EXECUTION_BOUNDARY,
        case,
    })?;

    Ok(PreparedCase {
        target_count_key,
        baseline,
        transactions: prepared_transactions,
        expected,
        baseline_hash,
        prepared_hash,
        declared_gas,
    })
}

pub(crate) fn execute_case(
    prepared: &PreparedCase,
    diagnostic: bool,
) -> Result<ExecutionObservation, String> {
    let precompiles = Precompiles::base(SpecId::OSAKA);
    let precompile_addresses = precompiles.addresses().into_iter().collect::<HashSet<_>>();
    let block = BlockEnv::<BaseEvmTypes>::default();
    let mut evm: BenchEvm = Evm::new(
        SpecId::OSAKA,
        block,
        ethereum_tx_registry(SpecId::OSAKA),
        prepared.baseline.clone(),
        precompiles,
    );
    if diagnostic {
        evm.set_inspector(ComputeInspector::new(precompile_addresses, &prepared.target_count_key));
    }
    let mut post = prepared.baseline.clone();
    let mut results = Vec::with_capacity(prepared.transactions.len());

    let started = Instant::now();
    for transaction in &prepared.transactions {
        let executed = evm
            .transact(transaction)
            .map_err(|error| format!("evm transaction failed: {error:?}"))?;
        let Ok(result) = executed.commit_with(&mut post);
        results.push(result);
    }
    let elapsed = started.elapsed().as_nanos().try_into().unwrap_or(u64::MAX).max(1);

    let counts = if diagnostic {
        let inspector = evm
            .clear_inspector_as::<ComputeInspector>()
            .ok_or_else(|| "diagnostic inspector was not installed".to_owned())?;
        Some(inspector.counts)
    } else {
        None
    };
    drop(evm);

    let receipts = results
        .iter()
        .map(|result| ExpectedReceipt {
            success: result.status,
            logs: result.logs.iter().map(observed_log).collect(),
        })
        .collect::<Vec<_>>();
    let mut correctness_error = verify_outcomes(&prepared.expected, &receipts, &post).err();
    let charged_gas = results.iter().try_fold(0_u64, |total, result| {
        total
            .checked_add(result.tx_gas_used())
            .ok_or_else(|| "charged transaction gas sum overflows u64".to_owned())
    })?;
    let commitment_hash =
        hash_json(&Commitment { receipts: &receipts, state: state_snapshot(&post) })?;
    let target_count =
        match counts.as_ref().map(|counts| target_count(prepared, counts)).transpose() {
            Ok(count) => count,
            Err(error) => {
                correctness_error = Some(match correctness_error {
                    Some(existing) => format!("{existing}; {error}"),
                    None => error,
                });
                None
            }
        };
    Ok(ExecutionObservation {
        duration_ns: elapsed,
        commitment_hash,
        charged_gas,
        opcode_counts: counts,
        target_count,
        correctness_error,
    })
}
fn target_count(prepared: &PreparedCase, counts: &BTreeMap<String, u64>) -> Result<u64, String> {
    counts.get(&prepared.target_count_key).copied().ok_or_else(|| {
        format!(
            "diagnostic execution did not observe target operation {}",
            prepared.target_count_key
        )
    })
}

fn observed_log(log: &alloy_primitives::Log) -> ExpectedLog {
    ExpectedLog {
        address: format!("{:#x}", log.address),
        topics: log.data.topics().iter().map(|topic| format!("{topic:#x}")).collect(),
        data: format!("0x{}", hex::encode(&log.data.data)),
    }
}

fn verify_outcomes(
    expected: &ExpectedOutcomes,
    receipts: &[ExpectedReceipt],
    post: &InMemoryDB,
) -> Result<(), String> {
    if expected.receipts != receipts {
        return Err(format!(
            "receipt outcomes differ: expected {:?}, got {:?}",
            expected.receipts, receipts
        ));
    }
    for (raw_address, slots) in &expected.storage {
        let address = parse_address(raw_address)?;
        for (raw_slot, raw_expected) in slots {
            let slot = parse_u256(raw_slot)?;
            let expected_value = parse_u256(raw_expected)?;
            let actual = post
                .cache
                .storage
                .get(&address)
                .and_then(|storage| storage.slots.get(&slot))
                .copied()
                .unwrap_or_default();
            if actual != expected_value {
                return Err(format!(
                    "storage mismatch at {address:#x}[{slot:#x}]: expected {expected_value:#x}, got {actual:#x}"
                ));
            }
        }
    }
    Ok(())
}

fn state_snapshot(database: &InMemoryDB) -> StateSnapshot {
    let accounts = database
        .cache
        .accounts
        .iter()
        .map(|(address, account)| {
            let account = account.as_ref().map(|account| AccountSnapshot {
                balance: format!("{:#066x}", account.balance),
                nonce: account.nonce,
                code_hash: format!("{:#x}", account.code_hash),
            });
            (format!("{address:#x}"), account)
        })
        .collect();
    let storage = database
        .cache
        .storage
        .iter()
        .map(|(address, account)| {
            let slots = account
                .slots
                .iter()
                .map(|(slot, value)| (format!("{slot:#066x}"), format!("{value:#066x}")))
                .collect();
            (format!("{address:#x}"), slots)
        })
        .collect();
    StateSnapshot { accounts, storage }
}

fn hash_json(value: &impl Serialize) -> Result<String, String> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| format!("serializing hash input: {error}"))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn parse_address(value: &str) -> Result<Address, String> {
    Address::from_str(value).map_err(|error| format!("invalid address {value:?}: {error}"))
}

fn parse_bytes(value: &str) -> Result<Bytes, String> {
    Bytes::from_str(value).map_err(|error| format!("invalid byte string {value:?}: {error}"))
}

fn parse_u256(value: &str) -> Result<U256, String> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    U256::from_str_radix(if digits.is_empty() { "0" } else { digits }, 16)
        .map_err(|error| format!("invalid hex quantity {value:?}: {error}"))
}

fn parse_u64(value: &str) -> Result<u64, String> {
    parse_u256(value)?.try_into().map_err(|_| format!("hex quantity {value:?} overflows u64"))
}
