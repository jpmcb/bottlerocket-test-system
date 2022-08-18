/*!

This test agent (in it's current form) runs a known SSM document.

Currently, the name of the SSM document is hardcoded.

TODO:
- [ ] Build and try this trivial mode
- [ ] make name of document configurable

!*/

mod os_test;

use crate::os_test::{
    check_for_ssm_document, ssm_run_doc, wait_for_ssm_ready,
};
use async_trait::async_trait;
use bottlerocket_agents::error::{self, Error};
use bottlerocket_agents::{aws_test_config, init_agent_logger};
use bottlerocket_types::agent_config::{OsConfig, AWS_CREDENTIALS_SECRET_NAME};
use log::{error, info};
use maplit::hashmap;
use model::{Outcome, SecretName, TestResults};
use snafu::ResultExt;
use std::path::Path;
use std::time::Duration;
use test_agent::{BootstrapData, ClientError, DefaultClient, Spec, TestAgent};

// This should be dynamically inferred from YAML config
const DOCUMENT_NAME: &str = "hello-world-command";

struct OsTestRunner {
    config: OsConfig,
    aws_secret_name: Option<SecretName>,
}

#[async_trait]
impl test_agent::Runner for OsTestRunner {
    type C = OsConfig;
    type E = Error;

    async fn new(spec: Spec<Self::C>) -> Result<Self, Self::E> {
        info!("Initializing os test agent...");
        Ok(Self {
            config: spec.configuration,
            aws_secret_name: spec.secrets.get(AWS_CREDENTIALS_SECRET_NAME).cloned(),
        })
    }

    async fn run(&mut self) -> Result<TestResults, Self::E> {
        let shared_config = aws_test_config(
            self,
            &self.aws_secret_name,
            &self.config.assume_role,
            &None,
            &Some(self.config.aws_region.clone()),
        )
        .await?;
        let ssm_client = aws_sdk_ssm::Client::new(&shared_config);

        // Ensure the SSM agents on the instances are ready, wait up to 5 minutes
        tokio::time::timeout(
            Duration::from_secs(300),
            wait_for_ssm_ready(&ssm_client, &self.config.instance_ids),
        )
        .await
        .context(error::SsmWaitInstanceReadyTimeoutSnafu)??;

        // Check if the SSM document to update Bottlerocket hosts exists, error if it does not.
        check_for_ssm_document(
            &ssm_client,
            DOCUMENT_NAME,
        )
        .await?;

        // Run the SSM doc
        info!("Initiating OS test using SSM document {:?}", DOCUMENT_NAME);
        match ssm_run_doc(
            &ssm_client,
            &self.config.instance_ids,
            DOCUMENT_NAME.to_string(),
        )
        .await
        {
            Ok(_) => {
                info!(
                    "All instances successfully ran SSM document {}",
                    self.config.ssm_doc_name
                );
                Ok(TestResults {
                    outcome: Outcome::Pass,
                    num_passed: self.config.instance_ids.len() as u64,
                    num_failed: 0,
                    num_skipped: 0,
                    other_info: Some(format!(
                        "Instances '{:?}' successfully ran SSM document {}",
                        &self.config.instance_ids, &self.config.ssm_doc_name
                    )),
                })
            }
            Err(e) => match e {
                Error::SsmRunCommand {
                    document_name,
                    instance_ids,
                } => {
                    error!(
                        "Instance(s) '{:?}' failed to run document {}",
                        instance_ids, document_name
                    );
                    Ok(TestResults {
                        outcome: Outcome::Fail,
                        num_passed: (self.config.instance_ids.len() - instance_ids.len()) as u64,
                        num_failed: instance_ids.len() as u64,
                        num_skipped: 0,
                        other_info: Some(format!(
                            "Instance(s) '{:?}' successfully migrated to {}; Instance(s) '{:?}' failed to migrate",
                            &self.config.instance_ids, document_name, instance_ids
                        )),
                    })
                }
                _ => Err(e),
            },
        }
    }

    async fn terminate(&mut self) -> Result<(), Self::E> {
        // Nothing to clean-up
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    init_agent_logger(env!("CARGO_CRATE_NAME"), None);
    if let Err(e) = run().await {
        error!("{}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), test_agent::error::Error<ClientError, Error>> {
    let mut agent = TestAgent::<DefaultClient, OsTestRunner>::new(
        BootstrapData::from_env().unwrap_or_else(|_| BootstrapData {
            test_name: "os_test".to_string(),
        }),
    )
    .await?;
    agent.run().await
}
