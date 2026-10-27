mod config;
mod fabric;

use std::env;
use std::path::PathBuf;

use rust_edition_mod_bridge::{
    HelperConnection, HelperRequest, MAX_VALUE_BYTES, ResultStatus, Snapshot, StateReason,
    snapshot_json, state_json,
};

use crate::config::ValidatedConfig;
use crate::fabric::FabricRuntime;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Rust Edition Fabric host failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let mut connection = HelperConnection::connect_from_env()
        .map_err(|error| format!("could not authenticate with the game: {error}"))?;
    let config_path = env::var_os(rust_edition_mod_bridge::CHILD_CONFIG_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {}", rust_edition_mod_bridge::CHILD_CONFIG_ENV));
    let config_path = match config_path {
        Ok(path) => path,
        Err(error) => return Err(report_startup_failure(&mut connection, error)),
    };
    let config = match ValidatedConfig::load(&config_path) {
        Ok(config) => config,
        Err(error) => return Err(report_startup_failure(&mut connection, error)),
    };
    let scratch_dir = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            return Err(report_startup_failure(
                &mut connection,
                format!("could not read helper scratch directory: {error}"),
            ));
        }
    };
    let runtime = match FabricRuntime::start(&config, &scratch_dir).await {
        Ok(runtime) => runtime,
        Err(error) => return Err(report_startup_failure(&mut connection, error)),
    };
    connection
        .mark_ready()
        .map_err(|error| format!("could not report Fabric readiness: {error}"))?;

    loop {
        let request = match connection.receive() {
            Ok(request) => request,
            Err(error) => {
                let message = format!("could not receive the next game snapshot: {error}");
                let _ = connection.fail(&message);
                return Err(message);
            }
        };
        match request {
            HelperRequest::Shutdown => return Ok(()),
            HelperRequest::Tick { seq, snapshot } => {
                process_frame(&mut connection, &runtime, seq, snapshot, None).await?;
            }
            HelperRequest::State {
                seq,
                reason,
                snapshot,
            } => {
                process_frame(&mut connection, &runtime, seq, snapshot, Some(reason)).await?;
            }
        }
    }
}

async fn process_frame(
    connection: &mut HelperConnection,
    runtime: &FabricRuntime,
    seq: u64,
    snapshot: Snapshot,
    state_reason: Option<StateReason>,
) -> Result<(), String> {
    let json_result = if let Some(reason) = state_reason {
        state_json(seq, reason, &snapshot)
    } else {
        snapshot_json(seq, &snapshot)
    };
    let json = match json_result {
        Ok(json) => json,
        Err(error) => {
            let message = format!("could not encode game snapshot: {error}");
            let _ = connection.send_result(
                seq,
                ResultStatus::Failed,
                Some(bounded_text(&message, MAX_VALUE_BYTES)),
            );
            return Err(message);
        }
    };
    let dispatch_client_tick = state_reason.is_none();
    let (status, value) = match runtime
        .on_frame(&snapshot, &json, dispatch_client_tick)
        .await
    {
        Ok(Some(value)) if value.len() <= MAX_VALUE_BYTES => (ResultStatus::Ok, Some(value)),
        Ok(None) => (ResultStatus::Ok, None),
        Ok(Some(_)) => (
            ResultStatus::Failed,
            Some(format!(
                "mod callback returned more than {MAX_VALUE_BYTES} UTF-8 bytes"
            )),
        ),
        Err(error) => (
            ResultStatus::Failed,
            Some(bounded_text(&error, MAX_VALUE_BYTES)),
        ),
    };
    if let Err(error) = connection.send_result(seq, status, value) {
        let message = format!("could not send mod callback result: {error}");
        let _ = connection.fail(&message);
        return Err(message);
    }
    if status == ResultStatus::Failed {
        return Err("mod callback failed; the game has the error result".into());
    }
    Ok(())
}

fn report_startup_failure(connection: &mut HelperConnection, message: String) -> String {
    if let Err(error) = connection.fail(&message) {
        format!("{message}; could not send the startup error: {error}")
    } else {
        message
    }
}

fn bounded_text(message: &str, maximum: usize) -> String {
    if message.len() <= maximum {
        return message.to_owned();
    }
    let mut end = maximum;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}
