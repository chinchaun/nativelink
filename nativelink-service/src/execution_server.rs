// Copyright 2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::unfold;
use futures::Stream;
use nativelink_config::cas_server::{ExecutionConfig, InstanceName};
use nativelink_error::{make_input_err, Error, ResultExt};
use nativelink_proto::build::bazel::remote::execution::v2::execution_server::{
    Execution, ExecutionServer as Server,
};
use nativelink_proto::build::bazel::remote::execution::v2::{
    Action, Command, ExecuteRequest, WaitExecutionRequest,
};
use nativelink_proto::google::longrunning::Operation;
use nativelink_scheduler::action_scheduler::{ActionListener, ActionScheduler};
use nativelink_store::ac_utils::get_and_decode_digest;
use nativelink_store::store_manager::StoreManager;
use nativelink_util::action_messages::{
    ActionInfo, ActionUniqueKey, ActionUniqueQualifier, ClientOperationId,
    DEFAULT_EXECUTION_PRIORITY,
};
use nativelink_util::common::DigestInfo;
use nativelink_util::digest_hasher::{make_ctx_for_hash_func, DigestHasherFunc};
use nativelink_util::platform_properties::PlatformProperties;
use nativelink_util::store_trait::Store;
use tonic::{Request, Response, Status};
use tracing::{error_span, event, instrument, Level};

type InstanceInfoName = String;

struct NativelinkClientOperationId {
    instance_name: InstanceInfoName,
    client_operation_id: ClientOperationId,
}

impl NativelinkClientOperationId {
    fn from_name(name: &str) -> Result<Self, Error> {
        let (instance_name, name) = name
            .split_once('/')
            .err_tip(|| "Expected instance_name and name to be separated by '/'")?;
        Ok(Self {
            instance_name: instance_name.to_string(),
            client_operation_id: ClientOperationId::from_raw_string(name.to_string()),
        })
    }

    fn into_string(self) -> String {
        format!(
            "{}/{}",
            self.instance_name,
            self.client_operation_id.into_string()
        )
    }
}

struct InstanceInfo {
    scheduler: Arc<dyn ActionScheduler>,
    cas_store: Store,
}

impl InstanceInfo {
    async fn build_action_info(
        &self,
        instance_name: String,
        action_digest: DigestInfo,
        action: &Action,
        priority: i32,
        skip_cache_lookup: bool,
        digest_function: DigestHasherFunc,
    ) -> Result<ActionInfo, Error> {
        let command_digest = DigestInfo::try_from(
            action
                .command_digest
                .clone()
                .err_tip(|| "Expected command_digest to exist")?,
        )
        .err_tip(|| "Could not decode command digest")?;

        let input_root_digest = DigestInfo::try_from(
            action
                .clone()
                .input_root_digest
                .err_tip(|| "Expected input_digest_root")?,
        )?;
        let timeout = action
            .timeout
            .map(|v| Duration::new(v.seconds as u64, v.nanos as u32))
            .unwrap_or(Duration::MAX);

        let mut platform_properties = HashMap::new();
        if let Some(platform) = &action.platform {
            for property in &platform.properties {
                let platform_property = self
                    .scheduler
                    .get_platform_property_manager(&instance_name)
                    .await
                    .err_tip(|| "Failed to get platform properties in build_action_info")?
                    .make_prop_value(&property.name, &property.value)
                    .err_tip(|| "Failed to convert platform property in build_action_info")?;
                platform_properties.insert(property.name.clone(), platform_property);
            }
        }

        // Goma puts the properties in the Command.
        if platform_properties.is_empty() {
            let command =
                get_and_decode_digest::<Command>(&self.cas_store, command_digest.into()).await?;
            if let Some(platform) = &command.platform {
                for property in &platform.properties {
                    let platform_property = self
                        .scheduler
                        .get_platform_property_manager(&instance_name)
                        .await
                        .err_tip(|| "Failed to get platform properties in build_action_info")?
                        .make_prop_value(&property.name, &property.value)
                        .err_tip(|| {
                            "Failed to convert command platform property in build_action_info"
                        })?;
                    platform_properties.insert(property.name.clone(), platform_property);
                }
            }
        }

        let action_key = ActionUniqueKey {
            instance_name,
            digest_function,
            digest: action_digest,
        };
        let unique_qualifier = if skip_cache_lookup {
            ActionUniqueQualifier::Uncachable(action_key)
        } else {
            ActionUniqueQualifier::Cachable(action_key)
        };

        Ok(ActionInfo {
            command_digest,
            input_root_digest,
            timeout,
            platform_properties: PlatformProperties::new(platform_properties),
            priority,
            load_timestamp: UNIX_EPOCH,
            insert_timestamp: SystemTime::now(),
            unique_qualifier,
        })
    }
}

pub struct ExecutionServer {
    instance_infos: HashMap<InstanceName, InstanceInfo>,
}

type ExecuteStream = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send + 'static>>;

impl ExecutionServer {
    pub fn new(
        config: &HashMap<InstanceName, ExecutionConfig>,
        scheduler_map: &HashMap<String, Arc<dyn ActionScheduler>>,
        store_manager: &StoreManager,
    ) -> Result<Self, Error> {
        let mut instance_infos = HashMap::with_capacity(config.len());
        for (instance_name, exec_cfg) in config {
            let cas_store = store_manager
                .get_store(&exec_cfg.cas_store)
                .ok_or_else(|| {
                    make_input_err!("'cas_store': '{}' does not exist", exec_cfg.cas_store)
                })?;
            let scheduler = scheduler_map
                .get(&exec_cfg.scheduler)
                .err_tip(|| {
                    format!(
                        "Scheduler needs config for '{}' because it exists in execution",
                        exec_cfg.scheduler
                    )
                })?
                .clone();

            instance_infos.insert(
                instance_name.to_string(),
                InstanceInfo {
                    scheduler,
                    cas_store,
                },
            );
        }
        Ok(Self { instance_infos })
    }

    pub fn into_service(self) -> Server<ExecutionServer> {
        Server::new(self)
    }

    fn to_execute_stream(
        nl_client_operation_id: NativelinkClientOperationId,
        action_listener: Pin<Box<dyn ActionListener>>,
    ) -> Response<ExecuteStream> {
        let client_operation_id_string = nl_client_operation_id.into_string();
        let receiver_stream = Box::pin(unfold(
            Some(action_listener),
            move |maybe_action_listener| {
                let client_operation_id_string = client_operation_id_string.clone();
                async move {
                    let mut action_listener = maybe_action_listener?;
                    match action_listener.changed().await {
                        Ok(action_update) => {
                            event!(Level::INFO, ?action_update, "Execute Resp Stream");
                            let client_operation_id = ClientOperationId::from_raw_string(
                                client_operation_id_string.clone(),
                            );
                            // If the action is finished we won't be sending any more updates.
                            let maybe_action_listener = if action_update.stage.is_finished() {
                                None
                            } else {
                                Some(action_listener)
                            };
                            Some((
                                Ok(action_update.as_operation(client_operation_id)),
                                maybe_action_listener,
                            ))
                        }
                        Err(err) => {
                            event!(Level::ERROR, ?err, "Error in action_listener stream");
                            Some((Err(err.into()), None))
                        }
                    }
                }
            },
        ));
        tonic::Response::new(receiver_stream)
    }

    async fn inner_execute(
        &self,
        request: ExecuteRequest,
    ) -> Result<Response<ExecuteStream>, Error> {
        let instance_name = request.instance_name;

        let instance_info = self
            .instance_infos
            .get(&instance_name)
            .err_tip(|| "Instance name '{}' not configured")?;

        let digest = DigestInfo::try_from(
            request
                .action_digest
                .err_tip(|| "Expected action_digest to exist")?,
        )
        .err_tip(|| "Failed to unwrap action cache")?;

        let priority = request
            .execution_policy
            .map_or(DEFAULT_EXECUTION_PRIORITY, |p| p.priority);

        let action =
            get_and_decode_digest::<Action>(&instance_info.cas_store, digest.into()).await?;
        let action_info = instance_info
            .build_action_info(
                instance_name.clone(),
                digest,
                &action,
                priority,
                request.skip_cache_lookup,
                request
                    .digest_function
                    .try_into()
                    .err_tip(|| "Could not convert digest function in inner_execute()")?,
            )
            .await?;

        let action_listener = instance_info
            .scheduler
            .add_action(
                ClientOperationId::new(action_info.unique_qualifier.clone()),
                action_info,
            )
            .await
            .err_tip(|| "Failed to schedule task")?;

        Ok(Self::to_execute_stream(
            NativelinkClientOperationId {
                instance_name,
                client_operation_id: action_listener.client_operation_id().clone(),
            },
            action_listener,
        ))
    }

    async fn inner_wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<ExecuteStream>, Status> {
        let (instance_name, client_operation_id) =
            NativelinkClientOperationId::from_name(&request.into_inner().name)
                .map(|v| (v.instance_name, v.client_operation_id))
                .err_tip(|| "Failed to parse operation_id in ExecutionServer::wait_execution")?;
        let Some(instance_info) = self.instance_infos.get(&instance_name) else {
            return Err(Status::not_found(format!(
                "No scheduler with the instance name {instance_name}"
            )));
        };
        let Some(rx) = instance_info
            .scheduler
            .find_by_client_operation_id(&client_operation_id)
            .await
            .err_tip(|| "Error running find_existing_action in ExecutionServer::wait_execution")?
        else {
            return Err(Status::not_found("Failed to find existing task"));
        };
        Ok(Self::to_execute_stream(
            NativelinkClientOperationId {
                instance_name,
                client_operation_id,
            },
            rx,
        ))
    }
}

#[tonic::async_trait]
impl Execution for ExecutionServer {
    type ExecuteStream = ExecuteStream;
    type WaitExecutionStream = ExecuteStream;

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(
        err,
        level = Level::ERROR,
        skip_all,
        fields(request = ?grpc_request.get_ref())
    )]
    async fn execute(
        &self,
        grpc_request: Request<ExecuteRequest>,
    ) -> Result<Response<ExecuteStream>, Status> {
        let request = grpc_request.into_inner();
        make_ctx_for_hash_func(request.digest_function)
            .err_tip(|| "In ExecutionServer::execute")?
            .wrap_async(
                error_span!("execution_server_execute"),
                self.inner_execute(request),
            )
            .await
            .err_tip(|| "Failed on execute() command")
            .map_err(|e| e.into())
    }

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(
        err,
        level = Level::ERROR,
        skip_all,
        fields(request = ?grpc_request.get_ref())
    )]
    async fn wait_execution(
        &self,
        grpc_request: Request<WaitExecutionRequest>,
    ) -> Result<Response<ExecuteStream>, Status> {
        let resp = self
            .inner_wait_execution(grpc_request)
            .await
            .err_tip(|| "Failed on wait_execution() command")
            .map_err(|e| e.into());

        if resp.is_ok() {
            event!(Level::DEBUG, return = "Ok(<stream>)");
        }
        resp
    }
}
