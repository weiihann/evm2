//! Finite Osaka compute benchmark worker for benchmarkoor campaigns.

mod protocol;
mod worker;

use clap::Parser;
use protocol::{
    EXECUTION_BOUNDARY, Mode, OSAKA_FORK, Request, ResultError, ResultRecord, SCHEMA_VERSION,
    SampleSpec, Workload,
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
};
use worker::{PreparedCase, execute_case, prepare_case};

#[derive(Debug, Parser)]
#[command(about = "Execute a finite benchmarkoor Osaka compute session")]
struct Args {
    /// Benchmarkoor worker request JSON.
    #[arg(long)]
    request: PathBuf,
    /// Destination JSONL ledger. The file must not already exist.
    #[arg(long)]
    output: PathBuf,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            let mut source = error.source();
            while let Some(error) = source {
                eprintln!("caused by: {error}");
                source = error.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let request_bytes = fs::read(&args.request)?;
    let request: Request = serde_json::from_slice(&request_bytes)?;
    validate_request(&request)?;

    let file = OpenOptions::new().create_new(true).write(true).open(&args.output)?;
    let mut output = BufWriter::new(file);
    let workload = match load_workload(&request) {
        Ok(workload) => workload,
        Err(message) => {
            for sample in &request.samples {
                write_record(&mut output, &failed(&request, sample, "workload", message.clone()))?;
            }
            return Ok(());
        }
    };
    let cases =
        workload.cases.iter().map(|case| (case.id.as_str(), case)).collect::<HashMap<_, _>>();
    let mut prepared = HashMap::<String, Result<PreparedCase, String>>::new();

    for sample in &request.samples {
        let Some(case) = cases.get(sample.case_id.as_str()) else {
            write_record(
                &mut output,
                &failed(
                    &request,
                    sample,
                    "workload",
                    format!("sample references unknown workload case {:?}", sample.case_id),
                ),
            )?;
            continue;
        };
        if case.status == "unsupported" {
            write_record(
                &mut output,
                &unsupported(
                    &request,
                    sample,
                    case.reason.clone().unwrap_or_else(|| "case is unsupported".to_owned()),
                ),
            )?;
            continue;
        }

        let preparation = prepared.entry(case.id.clone()).or_insert_with(|| prepare_case(case));
        let prepared_case = match preparation {
            Ok(prepared_case) => prepared_case,
            Err(message) => {
                write_record(&mut output, &failed(&request, sample, "prepare", message.clone()))?;
                continue;
            }
        };
        let observation = match execute_case(prepared_case, request.mode == Mode::Diagnostic) {
            Ok(observation) => observation,
            Err(message) => {
                let mut record = failed(&request, sample, "execute", message);
                record.baseline_hash = Some(prepared_case.baseline_hash.clone());
                record.prepared_hash = Some(prepared_case.prepared_hash.clone());
                record.declared_gas = Some(prepared_case.declared_gas);
                write_record(&mut output, &record)?;
                continue;
            }
        };
        let duration = (request.mode == Mode::Performance).then_some(observation.duration_ns);
        if let Some(message) = observation.correctness_error {
            write_record(
                &mut output,
                &ResultRecord {
                    schema_version: SCHEMA_VERSION,
                    session_id: request.session_id.clone(),
                    sample_id: sample.sample_id.clone(),
                    case_id: sample.case_id.clone(),
                    repetition: sample.repetition,
                    phase: sample.phase.clone(),
                    status: "failed",
                    execution_duration_ns: duration,
                    execution_boundary: EXECUTION_BOUNDARY,
                    baseline_hash: Some(prepared_case.baseline_hash.clone()),
                    prepared_hash: Some(prepared_case.prepared_hash.clone()),
                    commitment_hash: Some(observation.commitment_hash),
                    correctness_passed: false,
                    target_count: observation.target_count,
                    opcode_counts: observation.opcode_counts,
                    declared_gas: Some(prepared_case.declared_gas),
                    charged_gas: Some(observation.charged_gas),
                    error: Some(ResultError { stage: "correctness", message }),
                },
            )?;
            continue;
        }
        write_record(
            &mut output,
            &ResultRecord {
                schema_version: SCHEMA_VERSION,
                session_id: request.session_id.clone(),
                sample_id: sample.sample_id.clone(),
                case_id: sample.case_id.clone(),
                repetition: sample.repetition,
                phase: sample.phase.clone(),
                status: "executed",
                execution_duration_ns: duration,
                execution_boundary: EXECUTION_BOUNDARY,
                baseline_hash: Some(prepared_case.baseline_hash.clone()),
                prepared_hash: Some(prepared_case.prepared_hash.clone()),
                commitment_hash: Some(observation.commitment_hash),
                correctness_passed: true,
                target_count: observation.target_count,
                opcode_counts: observation.opcode_counts,
                declared_gas: Some(prepared_case.declared_gas),
                charged_gas: Some(observation.charged_gas),
                error: None,
            },
        )?;
    }
    Ok(())
}

fn load_workload(request: &Request) -> Result<Workload, String> {
    let bytes = fs::read(&request.workload_path)
        .map_err(|error| format!("reading workload {:?}: {error}", request.workload_path))?;
    let workload: Workload = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parsing workload {:?}: {error}", request.workload_path))?;
    if workload.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "workload schema_version {} is unsupported; expected {SCHEMA_VERSION}",
            workload.schema_version
        ));
    }
    if workload.fork != OSAKA_FORK {
        return Err(format!(
            "workload fork {:?} is unsupported; expected {OSAKA_FORK:?}",
            workload.fork
        ));
    }
    if workload.generator.revision.trim().is_empty() {
        return Err("workload generator revision is empty".to_owned());
    }
    let _generator_seed = workload.generator.seed;
    if workload.cases.is_empty() {
        return Err("workload contains no cases".to_owned());
    }
    let mut ids = HashSet::with_capacity(workload.cases.len());
    for case in &workload.cases {
        if case.id.is_empty() {
            return Err("workload contains a case with an empty id".to_owned());
        }
        if !ids.insert(case.id.as_str()) {
            return Err(format!("workload contains duplicate case id {:?}", case.id));
        }
    }
    Ok(workload)
}

fn validate_request(request: &Request) -> Result<(), Box<dyn Error>> {
    if request.schema_version != SCHEMA_VERSION {
        return Err(other(format!(
            "request schema_version {} is unsupported; expected {SCHEMA_VERSION}",
            request.schema_version
        )));
    }
    if request.workload_path.is_empty()
        || request.session_id.is_empty()
        || request.samples.is_empty()
    {
        return Err(other("request requires workload_path, session_id, and at least one sample"));
    }
    let mut ids = HashSet::with_capacity(request.samples.len());
    for sample in &request.samples {
        if sample.sample_id.is_empty() || sample.case_id.is_empty() {
            return Err(other("request samples require non-empty sample_id and case_id"));
        }
        if !ids.insert(sample.sample_id.as_str()) {
            return Err(other(format!("duplicate sample_id {:?}", sample.sample_id)));
        }
        let diagnostic_phase = sample.phase == "diagnostic";
        let timing_phase = matches!(sample.phase.as_str(), "pilot" | "warmup" | "qualification");
        if !diagnostic_phase && !timing_phase {
            return Err(other(format!("unknown sample phase {:?}", sample.phase)));
        }
        if diagnostic_phase != (request.mode == Mode::Diagnostic) {
            return Err(other(format!(
                "sample {:?} phase {:?} is incompatible with request mode {:?}",
                sample.sample_id, sample.phase, request.mode
            )));
        }
    }
    Ok(())
}

fn write_record(
    output: &mut BufWriter<std::fs::File>,
    record: &ResultRecord,
) -> Result<(), Box<dyn Error>> {
    serde_json::to_writer(&mut *output, record)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn failed(
    request: &Request,
    sample: &SampleSpec,
    stage: &'static str,
    message: String,
) -> ResultRecord {
    ResultRecord {
        schema_version: SCHEMA_VERSION,
        session_id: request.session_id.clone(),
        sample_id: sample.sample_id.clone(),
        case_id: sample.case_id.clone(),
        repetition: sample.repetition,
        phase: sample.phase.clone(),
        status: "failed",
        execution_duration_ns: None,
        execution_boundary: EXECUTION_BOUNDARY,
        baseline_hash: None,
        prepared_hash: None,
        commitment_hash: None,
        correctness_passed: false,
        target_count: None,
        opcode_counts: None,
        declared_gas: None,
        charged_gas: None,
        error: Some(ResultError { stage, message }),
    }
}

fn unsupported(request: &Request, sample: &SampleSpec, message: String) -> ResultRecord {
    let mut record = failed(request, sample, "workload", message);
    record.status = "unsupported";
    record
}

fn other(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}
