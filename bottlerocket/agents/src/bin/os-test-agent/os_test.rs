use aws_sdk_ec2::types::SdkError;
use aws_sdk_ssm::error::DescribeDocumentErrorKind;
use aws_sdk_ssm::model::{
    CommandInvocation, CommandInvocationStatus, InstanceInformationStringFilter,
};
use bottlerocket_agents::error;
use log::debug;
use maplit::hashmap;
use snafu::{OptionExt, ResultExt};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

/// Waits for the SSM agent to become ready on the instances
pub(crate) async fn wait_for_ssm_ready(
    ssm_client: &aws_sdk_ssm::Client,
    instance_ids: &HashSet<String>,
) -> Result<(), error::Error> {
    let mut num_ready = 0;
    let sec_between_checks = Duration::from_secs(6);
    while num_ready != instance_ids.len() {
        let instance_info = ssm_client
            .describe_instance_information()
            .filters(
                InstanceInformationStringFilter::builder()
                    .key("InstanceIds")
                    .set_values(Some(instance_ids.iter().cloned().collect::<Vec<_>>()))
                    .build(),
            )
            .send()
            .await
            .context(error::SsmDescribeInstanceInfoSnafu)?;
        num_ready = instance_info
            .instance_information_list()
            .map(|list| list.len())
            .context(error::SsmInstanceInfoSnafu)?;
        sleep(sec_between_checks);
    }

    Ok(())
}

/// Creates an SSM document if it doesn't already exist with the given name; if
/// it does but doesn't match the SSM document at given file path, updates it.
pub(crate) async fn check_for_ssm_document(
    ssm_client: &aws_sdk_ssm::Client,
    document_name: &str,
) -> Result<(), error::Error> {
    // Get the hash of the SSM document (if it exists already)
    match ssm_client
        .describe_document()
        .name(document_name)
        .send()
        .await
    {
        Ok(doc) => return Ok(()),
        Err(e) => return error::AwsSdkSnafu {
            message: e.to_string(),
        }
        .fail(),
    };
}

async fn wait_command_finish(
    ssm_client: &aws_sdk_ssm::Client,
    cmd_id: String,
) -> Result<Vec<CommandInvocation>, error::Error> {
    let seconds_between_checks = Duration::from_secs(2);
    loop {
        let cmd_status = ssm_client
            .list_command_invocations()
            .command_id(cmd_id.to_owned())
            .send()
            .await
            .context(error::SsmListCommandInvocationsSnafu)?;
        if let Some(invocations) = cmd_status.command_invocations {
            if invocations.is_empty()
                || invocations.iter().any(|i| {
                    matches!(
                        i.status,
                        Some(CommandInvocationStatus::InProgress)
                            | Some(CommandInvocationStatus::Pending)
                            | Some(CommandInvocationStatus::Delayed)
                    )
                })
            {
                // Command not finished, wait then check again
                sleep(seconds_between_checks)
            } else {
                return Ok(invocations);
            }
        }
    }
}

/// Runs a specified SSM document with specified parameters on provided list of instances
pub(crate) async fn ssm_run_doc(
    ssm_client: &aws_sdk_ssm::Client,
    instance_ids: &HashSet<String>,
    document_name: String,
) -> Result<Vec<CommandInvocation>, error::Error> {
    let cmd_id = ssm_client
        .send_command()
        .set_instance_ids(Some(instance_ids.iter().map(|i| i.to_owned()).collect()))
        .document_name(document_name.to_owned())
        .timeout_seconds(30)
        .send()
        .await
        .context(error::SsmSendCommandSnafu)?
        .command()
        .and_then(|c| c.command_id().map(|s| s.to_string()))
        .context(error::SsmCommandIdSnafu)?;

    debug!("############## Sent command, command ID: {}", cmd_id);
    // Wait for the command to finish
    if let Ok(invocations_result) = tokio::time::timeout(
        Duration::from_secs(60),
        wait_command_finish(ssm_client, cmd_id),
    )
    .await
    {
        let invocations = invocations_result?;
        for i in &invocations {
            debug!(
                "Instance: {}, Command Status: {}, Command Output: {:?}",
                i.instance_id.to_owned().unwrap_or_default(),
                i.status.as_ref().map(|s| s.as_str()).unwrap_or_default(),
                i.command_plugins
                    .to_owned()
                    .unwrap_or_default()
                    .iter()
                    .map(|c| c.output.as_ref().map(|s| s.to_string()).unwrap_or_default())
                    .collect::<Vec<String>>()
            )
        }
        let failed_invocations: Vec<_> = invocations
            .iter()
            .filter(|i| i.status != Some(CommandInvocationStatus::Success))
            .collect();
        if !failed_invocations.is_empty() {
            return error::SsmRunCommandSnafu {
                document_name,
                instance_ids: failed_invocations
                    .iter()
                    .map(|i| i.instance_id.to_owned().unwrap_or_default())
                    .collect::<Vec<String>>(),
            }
            .fail();
        }
        Ok(invocations)
    } else {
        // Timed-out waiting for commands to finish
        error::SsmWaitCommandTimeoutSnafu.fail()
    }
}

