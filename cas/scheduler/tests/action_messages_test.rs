// Copyright 2022 The Turbo Cache Authors. All rights reserved.
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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use action_messages::{ActionInfo, ActionInfoHashKey, ActionResult, ActionStage, ActionState, ExecutionMetadata};
use common::DigestInfo;
use error::Error;
use platform_property_manager::PlatformProperties;
use proto::build::bazel::remote::execution::v2::ExecuteResponse;
use proto::google::longrunning::{operation, Operation};
use proto::google::rpc::Status;

const NOW_TIME: u64 = 10000;

fn make_system_time(add_time: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(NOW_TIME + add_time))
        .unwrap()
}

#[cfg(test)]
mod action_messages_tests {
    use super::*;
    use pretty_assertions::assert_eq; // Must be declared in every module.

    #[tokio::test]
    async fn action_state_any_url_test() -> Result<(), Error> {
        let operation: Operation = ActionState {
            name: "test".to_string(),
            action_digest: DigestInfo::new([1u8; 32], 5),
            stage: ActionStage::Unknown,
        }
        .into();

        match operation.result {
            Some(operation::Result::Response(any)) => assert_eq!(
                any.type_url,
                "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
            ),
            other => assert!(false, "Expected Some(Result(Any)), got: {:?}", other),
        }

        Ok(())
    }

    #[tokio::test]
    async fn execute_response_status_message_is_some_on_success_test() -> Result<(), Error> {
        let execute_response: ExecuteResponse = ActionStage::Completed(ActionResult {
            output_files: vec![],
            output_folders: vec![],
            output_file_symlinks: vec![],
            output_directory_symlinks: vec![],
            exit_code: 0,
            stdout_digest: DigestInfo::new([2u8; 32], 5),
            stderr_digest: DigestInfo::new([3u8; 32], 5),
            execution_metadata: ExecutionMetadata {
                worker: "foo_worker_id".to_string(),
                queued_timestamp: SystemTime::UNIX_EPOCH,
                worker_start_timestamp: SystemTime::UNIX_EPOCH,
                worker_completed_timestamp: SystemTime::UNIX_EPOCH,
                input_fetch_start_timestamp: SystemTime::UNIX_EPOCH,
                input_fetch_completed_timestamp: SystemTime::UNIX_EPOCH,
                execution_start_timestamp: SystemTime::UNIX_EPOCH,
                execution_completed_timestamp: SystemTime::UNIX_EPOCH,
                output_upload_start_timestamp: SystemTime::UNIX_EPOCH,
                output_upload_completed_timestamp: SystemTime::UNIX_EPOCH,
            },
            server_logs: Default::default(),
        })
        .into();

        // This was once discovered to be None, which is why this test exists.
        assert_eq!(execute_response.status, Some(Status::default()));

        Ok(())
    }

    #[tokio::test]
    async fn highest_priority_action_first() -> Result<(), Error> {
        const INSTANCE_NAME: &str = "foobar_instance_name";

        let high_priority_action = Arc::new(ActionInfo {
            instance_name: INSTANCE_NAME.to_string(),
            command_digest: DigestInfo::new([0u8; 32], 0),
            input_root_digest: DigestInfo::new([0u8; 32], 0),
            timeout: Duration::MAX,
            platform_properties: PlatformProperties {
                properties: HashMap::new(),
            },
            priority: 1000,
            load_timestamp: SystemTime::UNIX_EPOCH,
            insert_timestamp: SystemTime::UNIX_EPOCH,
            unique_qualifier: ActionInfoHashKey {
                digest: DigestInfo::new([0u8; 32], 0),
                salt: 0,
            },
        });
        let lowest_priority_action = Arc::new(ActionInfo {
            instance_name: INSTANCE_NAME.to_string(),
            command_digest: DigestInfo::new([0u8; 32], 0),
            input_root_digest: DigestInfo::new([0u8; 32], 0),
            timeout: Duration::MAX,
            platform_properties: PlatformProperties {
                properties: HashMap::new(),
            },
            priority: 0,
            load_timestamp: SystemTime::UNIX_EPOCH,
            insert_timestamp: SystemTime::UNIX_EPOCH,
            unique_qualifier: ActionInfoHashKey {
                digest: DigestInfo::new([1u8; 32], 0),
                salt: 0,
            },
        });
        let mut action_map = BTreeMap::<Arc<ActionInfo>, ()>::new();
        action_map.insert(lowest_priority_action.clone(), ());
        action_map.insert(high_priority_action.clone(), ());

        assert_eq!(
            vec![high_priority_action, lowest_priority_action],
            action_map.keys().rev().cloned().collect::<Vec<Arc<ActionInfo>>>()
        );

        Ok(())
    }

    #[tokio::test]
    async fn equal_priority_earliest_first() -> Result<(), Error> {
        const INSTANCE_NAME: &str = "foobar_instance_name";

        let first_action = Arc::new(ActionInfo {
            instance_name: INSTANCE_NAME.to_string(),
            command_digest: DigestInfo::new([0u8; 32], 0),
            input_root_digest: DigestInfo::new([0u8; 32], 0),
            timeout: Duration::MAX,
            platform_properties: PlatformProperties {
                properties: HashMap::new(),
            },
            priority: 0,
            load_timestamp: SystemTime::UNIX_EPOCH,
            insert_timestamp: SystemTime::UNIX_EPOCH,
            unique_qualifier: ActionInfoHashKey {
                digest: DigestInfo::new([0u8; 32], 0),
                salt: 0,
            },
        });
        let current_action = Arc::new(ActionInfo {
            instance_name: INSTANCE_NAME.to_string(),
            command_digest: DigestInfo::new([0u8; 32], 0),
            input_root_digest: DigestInfo::new([0u8; 32], 0),
            timeout: Duration::MAX,
            platform_properties: PlatformProperties {
                properties: HashMap::new(),
            },
            priority: 0,
            load_timestamp: SystemTime::UNIX_EPOCH,
            insert_timestamp: make_system_time(0),
            unique_qualifier: ActionInfoHashKey {
                digest: DigestInfo::new([1u8; 32], 0),
                salt: 0,
            },
        });
        let mut action_map = BTreeMap::<Arc<ActionInfo>, ()>::new();
        action_map.insert(current_action.clone(), ());
        action_map.insert(first_action.clone(), ());

        assert_eq!(
            vec![first_action, current_action],
            action_map.keys().rev().cloned().collect::<Vec<Arc<ActionInfo>>>()
        );

        Ok(())
    }
}