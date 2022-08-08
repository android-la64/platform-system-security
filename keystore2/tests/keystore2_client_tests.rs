// Copyright 2022, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use nix::unistd::{getuid, Gid, Uid};
use rustutils::users::AID_USER_OFFSET;
use serde::{Deserialize, Serialize};

use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
    Algorithm::Algorithm, BlockMode::BlockMode, Digest::Digest, EcCurve::EcCurve,
    ErrorCode::ErrorCode, KeyParameterValue::KeyParameterValue, KeyPurpose::KeyPurpose,
    PaddingMode::PaddingMode, SecurityLevel::SecurityLevel, Tag::Tag,
};
use android_system_keystore2::aidl::android::system::keystore2::{
    CreateOperationResponse::CreateOperationResponse, Domain::Domain,
    IKeystoreOperation::IKeystoreOperation, IKeystoreSecurityLevel::IKeystoreSecurityLevel,
    KeyDescriptor::KeyDescriptor, KeyParameters::KeyParameters, KeyPermission::KeyPermission,
    ResponseCode::ResponseCode,
};

use keystore2_test_utils::authorizations;
use keystore2_test_utils::get_keystore_service;
use keystore2_test_utils::key_generations;
use keystore2_test_utils::key_generations::Error;
use keystore2_test_utils::run_as;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum TestOutcome {
    Ok,
    BackendBusy,
    InvalidHandle,
    OtherErr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BarrierReached;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ForcedOp(pub bool);

/// Sample plain text input for encrypt operation.
pub const SAMPLE_PLAIN_TEXT: &[u8] = b"my message 11111";

/// Generate a EC_P256 key using given domain, namespace and alias.
/// Create an operation using the generated key and perform sample signing operation.
fn create_signing_operation(
    forced_op: ForcedOp,
    op_purpose: KeyPurpose,
    op_digest: Digest,
    domain: Domain,
    nspace: i64,
    alias: Option<String>,
) -> binder::Result<CreateOperationResponse> {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_metadata = key_generations::generate_ec_p256_signing_key(
        &sec_level, domain, nspace, alias, None, None,
    )
    .unwrap();

    sec_level.createOperation(
        &key_metadata.key,
        &authorizations::AuthSetBuilder::new().purpose(op_purpose).digest(op_digest),
        forced_op.0,
    )
}

/// Generate an EC signing key and grant it to the user with given access vector.
fn generate_ec_key_and_grant_to_user(
    grantee_uid: i32,
    access_vector: i32,
) -> binder::Result<KeyDescriptor> {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("{}{}", "ks_grant_test_key_1", getuid());

    let key_metadata = key_generations::generate_ec_p256_signing_key(
        &sec_level,
        Domain::SELINUX,
        key_generations::SELINUX_SHELL_NAMESPACE,
        Some(alias),
        None,
        None,
    )
    .unwrap();

    keystore2.grant(&key_metadata.key, grantee_uid, access_vector)
}

/// Performs sample signing operation.
fn perform_sample_sign_operation(
    op: &binder::Strong<dyn IKeystoreOperation>,
) -> Result<(), binder::Status> {
    op.update(b"my message")?;
    let sig = op.finish(None, None)?;
    assert!(sig.is_some());
    Ok(())
}

/// Generate a RSA key and create an operation using the generated key.
fn create_rsa_key_and_operation(
    sec_level: &binder::Strong<dyn IKeystoreSecurityLevel>,
    domain: Domain,
    nspace: i64,
    alias: Option<String>,
    key_params: &key_generations::KeyParams,
    op_purpose: KeyPurpose,
    forced_op: ForcedOp,
) -> binder::Result<CreateOperationResponse> {
    let key_metadata =
        key_generations::generate_rsa_key(sec_level, domain, nspace, alias, key_params, None)?;

    let mut op_params = authorizations::AuthSetBuilder::new().purpose(op_purpose);

    if let Some(value) = key_params.digest {
        op_params = op_params.digest(value)
    }
    if let Some(value) = key_params.padding {
        op_params = op_params.padding_mode(value);
    }
    if let Some(value) = key_params.mgf_digest {
        op_params = op_params.mgf_digest(value);
    }
    if let Some(value) = key_params.block_mode {
        op_params = op_params.block_mode(value)
    }

    sec_level.createOperation(&key_metadata.key, &op_params, forced_op.0)
}

/// Get NONCE value from given key parameters list.
fn get_op_nonce(parameters: &KeyParameters) -> Option<Vec<u8>> {
    for key_param in &parameters.keyParameter {
        if key_param.tag == Tag::NONCE {
            if let KeyParameterValue::Blob(val) = &key_param.value {
                return Some(val.clone());
            }
        }
    }
    None
}

/// This performs sample encryption operation with given symmetric key (AES/3DES).
fn perform_sample_sym_key_encrypt_op(
    sec_level: &binder::Strong<dyn IKeystoreSecurityLevel>,
    padding_mode: PaddingMode,
    block_mode: BlockMode,
    nonce: &mut Option<Vec<u8>>,
    mac_len: Option<i32>,
    key: &KeyDescriptor,
) -> binder::Result<Option<Vec<u8>>> {
    let mut op_params = authorizations::AuthSetBuilder::new()
        .purpose(KeyPurpose::ENCRYPT)
        .padding_mode(padding_mode)
        .block_mode(block_mode);
    if let Some(value) = nonce {
        op_params = op_params.nonce(value.to_vec());
    }

    if let Some(val) = mac_len {
        op_params = op_params.mac_length(val);
    }

    let op_response = sec_level.createOperation(key, &op_params, false)?;
    assert!(op_response.iOperation.is_some());
    let op = op_response.iOperation.unwrap();
    if op_response.parameters.is_some() && nonce.is_none() {
        *nonce = get_op_nonce(&op_response.parameters.unwrap());
    }
    op.finish(Some(SAMPLE_PLAIN_TEXT), None)
}

/// This performs sample decryption operation with given symmetric key (AES/3DES).
fn perform_sample_sym_key_decrypt_op(
    sec_level: &binder::Strong<dyn IKeystoreSecurityLevel>,
    input: &[u8],
    padding_mode: PaddingMode,
    block_mode: BlockMode,
    nonce: &mut Option<Vec<u8>>,
    mac_len: Option<i32>,
    key: &KeyDescriptor,
) -> binder::Result<Option<Vec<u8>>> {
    let mut op_params = authorizations::AuthSetBuilder::new()
        .purpose(KeyPurpose::DECRYPT)
        .padding_mode(padding_mode)
        .block_mode(block_mode);
    if let Some(value) = nonce {
        op_params = op_params.nonce(value.to_vec());
    }

    if let Some(val) = mac_len {
        op_params = op_params.mac_length(val);
    }

    let op_response = sec_level.createOperation(key, &op_params, false)?;
    assert!(op_response.iOperation.is_some());
    let op = op_response.iOperation.unwrap();
    op.finish(Some(input), None)
}

/// Generate a AES key. Create encrypt and decrypt operations using the generated key.
fn create_aes_key_and_operation(
    sec_level: &binder::Strong<dyn IKeystoreSecurityLevel>,
    key_size: i32,
    padding_mode: PaddingMode,
    block_mode: BlockMode,
    mac_len: Option<i32>,
    min_mac_len: Option<i32>,
    nonce: &mut Option<Vec<u8>>,
) -> Result<(), binder::Status> {
    let alias = format!("ks_aes_test_key_{}{}{}", key_size, block_mode.0, padding_mode.0);

    let key_metadata = key_generations::generate_aes_key(
        sec_level,
        key_size,
        &alias,
        &padding_mode,
        &block_mode,
        min_mac_len,
    )?;

    let cipher_text = perform_sample_sym_key_encrypt_op(
        sec_level,
        padding_mode,
        block_mode,
        nonce,
        mac_len,
        &key_metadata.key,
    )?;

    assert!(cipher_text.is_some());

    let plain_text = perform_sample_sym_key_decrypt_op(
        sec_level,
        &cipher_text.unwrap(),
        padding_mode,
        block_mode,
        nonce,
        mac_len,
        &key_metadata.key,
    )
    .unwrap();
    assert!(plain_text.is_some());
    assert_eq!(plain_text.unwrap(), SAMPLE_PLAIN_TEXT.to_vec());
    Ok(())
}

/// Create new operation on child proc and perform simple operation after parent notification.
fn execute_op_run_as_child(
    target_ctx: &'static str,
    domain: Domain,
    nspace: i64,
    alias: Option<String>,
    auid: Uid,
    agid: Gid,
    forced_op: ForcedOp,
) -> run_as::ChildHandle<TestOutcome, BarrierReached> {
    unsafe {
        run_as::run_as_child(target_ctx, auid, agid, move |reader, writer| {
            let result = key_generations::map_ks_error(create_signing_operation(
                forced_op,
                KeyPurpose::SIGN,
                Digest::SHA_2_256,
                domain,
                nspace,
                alias,
            ));

            // Let the parent know that an operation has been started, then
            // wait until the parent notifies us to continue, so the operation
            // remains open.
            writer.send(&BarrierReached {});
            reader.recv();

            // Continue performing the operation after parent notifies.
            match &result {
                Ok(CreateOperationResponse { iOperation: Some(op), .. }) => {
                    match key_generations::map_ks_error(perform_sample_sign_operation(op)) {
                        Ok(()) => TestOutcome::Ok,
                        Err(Error::Km(ErrorCode::INVALID_OPERATION_HANDLE)) => {
                            TestOutcome::InvalidHandle
                        }
                        Err(e) => panic!("Error in performing op: {:#?}", e),
                    }
                }
                Ok(_) => TestOutcome::OtherErr,
                Err(Error::Rc(ResponseCode::BACKEND_BUSY)) => TestOutcome::BackendBusy,
                _ => TestOutcome::OtherErr,
            }
        })
        .expect("Failed to create an operation.")
    }
}

fn create_operations(
    target_ctx: &'static str,
    forced_op: ForcedOp,
    max_ops: i32,
) -> Vec<run_as::ChildHandle<TestOutcome, BarrierReached>> {
    let alias = format!("ks_op_test_key_{}", getuid());
    let base_gid = 99 * AID_USER_OFFSET + 10001;
    let base_uid = 99 * AID_USER_OFFSET + 10001;
    (0..max_ops)
        .into_iter()
        .map(|i| {
            execute_op_run_as_child(
                target_ctx,
                Domain::APP,
                key_generations::SELINUX_SHELL_NAMESPACE,
                Some(alias.to_string()),
                Uid::from_raw(base_uid + (i as u32)),
                Gid::from_raw(base_gid + (i as u32)),
                forced_op,
            )
        })
        .collect()
}

/// This test verifies that backend service throws BACKEND_BUSY error when all
/// operations slots are full. This test creates operations in child processes and
/// collects the status of operations performed in each child proc and determines
/// whether any child proc exited with error status.
#[test]
fn keystore2_backend_busy_test() {
    const MAX_OPS: i32 = 100;
    static TARGET_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";

    let mut child_handles = create_operations(TARGET_CTX, ForcedOp(false), MAX_OPS);

    // Wait until all child procs notifies us to continue,
    // so that there are definitely enough operations outstanding to trigger a BACKEND_BUSY.
    for ch in child_handles.iter_mut() {
        ch.recv();
    }
    // Notify each child to resume and finish.
    for ch in child_handles.iter_mut() {
        ch.send(&BarrierReached {});
    }

    // Collect the result and validate whether backend busy has occurred.
    let mut busy_count = 0;
    for ch in child_handles.into_iter() {
        if ch.get_result() == TestOutcome::BackendBusy {
            busy_count += 1;
        }
    }
    assert!(busy_count > 0)
}

/// This test confirms that forced operation is having high pruning power.
/// 1. Initially create regular operations such that there are enough operations outstanding
///    to trigger BACKEND_BUSY.
/// 2. Then, create a forced operation. System should be able to prune one of the regular
///    operations and create a slot for forced operation successfully.
#[test]
fn keystore2_forced_op_after_backendbusy_test() {
    const MAX_OPS: i32 = 100;
    static TARGET_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";

    // Create regular operations.
    let mut child_handles = create_operations(TARGET_CTX, ForcedOp(false), MAX_OPS);

    // Wait until all child procs notifies us to continue, so that there are enough
    // operations outstanding to trigger a BACKEND_BUSY.
    for ch in child_handles.iter_mut() {
        ch.recv();
    }

    // Create a forced operation.
    let auid = 99 * AID_USER_OFFSET + 10604;
    let agid = 99 * AID_USER_OFFSET + 10604;
    unsafe {
        run_as::run_as(
            key_generations::TARGET_VOLD_CTX,
            Uid::from_raw(auid),
            Gid::from_raw(agid),
            move || {
                let alias = format!("ks_prune_forced_op_key_{}", getuid());

                // To make room for this forced op, system should be able to prune one of the
                // above created regular operations and create a slot for this forced operation
                // successfully.
                create_signing_operation(
                    ForcedOp(true),
                    KeyPurpose::SIGN,
                    Digest::SHA_2_256,
                    Domain::SELINUX,
                    100,
                    Some(alias),
                )
                .expect("Client failed to create forced operation after BACKEND_BUSY state.");
            },
        );
    };

    // Notify each child to resume and finish.
    for ch in child_handles.iter_mut() {
        ch.send(&BarrierReached {});
    }

    // Collect the results of above created regular operations.
    let mut pruned_count = 0;
    let mut busy_count = 0;
    let mut _other_err = 0;
    for ch in child_handles.into_iter() {
        match ch.get_result() {
            TestOutcome::BackendBusy => {
                busy_count += 1;
            }
            TestOutcome::InvalidHandle => {
                pruned_count += 1;
            }
            _ => {
                _other_err += 1;
            }
        }
    }
    // Verify that there should be at least one backend busy has occurred while creating
    // above regular operations.
    assert!(busy_count > 0);

    // Verify that there should be at least one pruned operation which should have failed while
    // performing operation.
    assert!(pruned_count > 0);
}

/// This test confirms that forced operations can't be pruned.
///  1. Creates an initial forced operation and tries to complete the operation after BACKEND_BUSY
///     error is triggered.
///  2. Create MAX_OPS number of forced operations so that definitely enough number of operations
///     outstanding to trigger a BACKEND_BUSY.
///  3. Try to use initially created forced operation (in step #1) and able to perform the
///     operation successfully. This confirms that none of the later forced operations evicted the
///     initial forced operation.
#[test]
fn keystore2_max_forced_ops_test() {
    const MAX_OPS: i32 = 100;
    let auid = 99 * AID_USER_OFFSET + 10205;
    let agid = 99 * AID_USER_OFFSET + 10205;

    // Create initial forced operation in a child process
    // and wait for the parent to notify to perform operation.
    let alias = format!("ks_forced_op_key_{}", getuid());
    let mut first_op_handle = execute_op_run_as_child(
        key_generations::TARGET_SU_CTX,
        Domain::SELINUX,
        key_generations::SELINUX_SHELL_NAMESPACE,
        Some(alias),
        Uid::from_raw(auid),
        Gid::from_raw(agid),
        ForcedOp(true),
    );

    // Wait until above child proc notifies us to continue, so that there is definitely a forced
    // operation outstanding to perform a operation.
    first_op_handle.recv();

    // Create MAX_OPS number of forced operations.
    let mut child_handles =
        create_operations(key_generations::TARGET_SU_CTX, ForcedOp(true), MAX_OPS);

    // Wait until all child procs notifies us to continue, so that  there are enough operations
    // outstanding to trigger a BACKEND_BUSY.
    for ch in child_handles.iter_mut() {
        ch.recv();
    }

    // Notify initial created forced operation to continue performing the operations.
    first_op_handle.send(&BarrierReached {});

    // Collect initially created forced operation result and is expected to complete operation
    // successfully.
    let first_op_result = first_op_handle.get_result();
    assert_eq!(first_op_result, TestOutcome::Ok);

    // Notify each child to resume and finish.
    for ch in child_handles.iter_mut() {
        ch.send(&BarrierReached {});
    }

    // Collect the result and validate whether backend busy has occurred with MAX_OPS number
    // of forced operations.
    let busy_count = child_handles
        .into_iter()
        .map(|ch| ch.get_result())
        .filter(|r| *r == TestOutcome::BackendBusy)
        .count();
    assert!(busy_count > 0);
}

/// This test will verify the use case with the same owner(UID) requesting `n` number of operations.
/// This test confirms that when all operation slots are full and a new operation is requested,
/// an operation which is least recently used and lived longest will be pruned to make a room
/// for a new operation. Pruning strategy should prevent the operations of the other owners(UID)
/// from being pruned.
///
/// 1. Create an operation in a child process with `untrusted_app` context and wait for parent
///    notification to complete the operation.
/// 2. Let parent process create `n` number of operations such that there are enough operations
///    outstanding to trigger cannibalizing their own sibling operations.
/// 3. Sequentially try to use above created `n` number of operations and also add a new operation,
///    so that it should trigger cannibalizing one of their own sibling operations.
///    3.1 While trying to use these pruned operations an `INVALID_OPERATION_HANDLE` error is
///        expected as they are already pruned.
/// 4. Notify the child process to resume and complete the operation. It is expected to complete the
///    operation successfully.
/// 5. Try to use the latest operation of parent. It is expected to complete the operation
///    successfully.
#[test]
fn keystore2_ops_prune_test() {
    const MAX_OPS: usize = 40; // This should be at least 32 with sec_level TEE.

    static TARGET_CTX: &str = "u:r:untrusted_app:s0";
    const USER_ID: u32 = 99;
    const APPLICATION_ID: u32 = 10601;

    let uid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;
    let gid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;

    // Create an operation in an untrusted_app context. Wait until the parent notifies to continue.
    // Once the parent notifies, this operation is expected to be completed successfully.
    let alias = format!("ks_reg_op_key_{}", getuid());
    let mut child_handle = execute_op_run_as_child(
        TARGET_CTX,
        Domain::APP,
        -1,
        Some(alias),
        Uid::from_raw(uid),
        Gid::from_raw(gid),
        ForcedOp(false),
    );

    // Wait until child process notifies us to continue, so that an operation from child process is
    // outstanding to complete the operation.
    child_handle.recv();

    // Generate a key to use in below operations.
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("ks_prune_op_test_key_{}", getuid());
    let key_metadata = key_generations::generate_ec_p256_signing_key(
        &sec_level,
        Domain::SELINUX,
        key_generations::SELINUX_SHELL_NAMESPACE,
        Some(alias),
        None,
        None,
    )
    .unwrap();

    // Create multiple operations in this process to trigger cannibalizing sibling operations.
    let mut ops: Vec<binder::Result<CreateOperationResponse>> = (0..MAX_OPS)
        .into_iter()
        .map(|_| {
            sec_level.createOperation(
                &key_metadata.key,
                &authorizations::AuthSetBuilder::new()
                    .purpose(KeyPurpose::SIGN)
                    .digest(Digest::SHA_2_256),
                false,
            )
        })
        .collect();

    // Sequentially try to use operation handles created above and also add a new operation.
    for vec_index in 0..MAX_OPS {
        match &ops[vec_index] {
            Ok(CreateOperationResponse { iOperation: Some(op), .. }) => {
                // Older operation handle is pruned, if we try to use that an error is expected.
                assert_eq!(
                    Err(Error::Km(ErrorCode::INVALID_OPERATION_HANDLE)),
                    key_generations::map_ks_error(op.update(b"my message"))
                );
            }
            _ => panic!("Operation should have created successfully."),
        }

        // Create a new operation, it should trigger to cannibalize one of their own sibling
        // operations.
        ops.push(
            sec_level.createOperation(
                &key_metadata.key,
                &authorizations::AuthSetBuilder::new()
                    .purpose(KeyPurpose::SIGN)
                    .digest(Digest::SHA_2_256),
                false,
            ),
        );
    }

    // Notify child process to continue the operation.
    child_handle.send(&BarrierReached {});
    assert!((child_handle.get_result() == TestOutcome::Ok), "Failed to perform an operation");

    // Try to use the latest operation created by parent, should be able to use it successfully.
    match ops.last() {
        Some(Ok(CreateOperationResponse { iOperation: Some(op), .. })) => {
            assert_eq!(Ok(()), key_generations::map_ks_error(perform_sample_sign_operation(op)));
        }
        _ => panic!("Operation should have created successfully."),
    }
}

/// This test will try to load the key with Domain::BLOB.
/// INVALID_ARGUMENT error is expected.
#[test]
fn keystore2_get_key_entry_blob_fail() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    // Generate a key with domain as BLOB.
    let key_metadata = key_generations::generate_ec_p256_signing_key(
        &sec_level,
        Domain::BLOB,
        key_generations::SELINUX_SHELL_NAMESPACE,
        None,
        None,
        None,
    )
    .unwrap();

    // Try to load the key using above generated KeyDescriptor.
    let result = key_generations::map_ks_error(keystore2.getKeyEntry(&key_metadata.key));
    assert!(result.is_err());
    assert_eq!(Error::Rc(ResponseCode::INVALID_ARGUMENT), result.unwrap_err());

    // Delete the generated key blob.
    sec_level.deleteKey(&key_metadata.key).unwrap();
}

/// Try to create forced operations with various contexts -
///   - untrusted_app
///   - system_server
///   - priv_app
/// `PERMISSION_DENIED` error response is expected.
#[test]
fn keystore2_forced_op_perm_denied_test() {
    static TARGET_CTXS: &[&str] =
        &["u:r:untrusted_app:s0", "u:r:system_server:s0", "u:r:priv_app:s0"];
    const USER_ID: u32 = 99;
    const APPLICATION_ID: u32 = 10601;

    let uid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;
    let gid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;

    for context in TARGET_CTXS.iter() {
        unsafe {
            run_as::run_as(context, Uid::from_raw(uid), Gid::from_raw(gid), move || {
                let alias = format!("ks_app_forced_op_test_key_{}", getuid());
                let result = key_generations::map_ks_error(create_signing_operation(
                    ForcedOp(true),
                    KeyPurpose::SIGN,
                    Digest::SHA_2_256,
                    Domain::APP,
                    -1,
                    Some(alias),
                ));
                assert!(result.is_err());
                assert_eq!(Error::Rc(ResponseCode::PERMISSION_DENIED), result.unwrap_err());
            });
        }
    }
}

/// Try to create a forced operation with `vold` context.
/// Should be able to create forced operation with `vold` context successfully.
#[test]
fn keystore2_forced_op_success_test() {
    static TARGET_CTX: &str = "u:r:vold:s0";
    const USER_ID: u32 = 99;
    const APPLICATION_ID: u32 = 10601;

    let uid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;
    let gid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;

    unsafe {
        run_as::run_as(TARGET_CTX, Uid::from_raw(uid), Gid::from_raw(gid), move || {
            let alias = format!("ks_vold_forced_op_key_{}", getuid());
            create_signing_operation(
                ForcedOp(true),
                KeyPurpose::SIGN,
                Digest::SHA_2_256,
                Domain::SELINUX,
                key_generations::SELINUX_VOLD_NAMESPACE,
                Some(alias),
            )
            .expect("Client with vold context failed to create forced operation.");
        });
    }
}

/// Try to grant a key with permission that does not map to any of the `KeyPermission` values.
/// An error is expected with values that does not map to set of permissions listed in
/// `KeyPermission`.
#[test]
fn keystore2_grant_key_with_invalid_perm_expecting_syserror() {
    const USER_ID: u32 = 99;
    const APPLICATION_ID: u32 = 10001;
    let grantee_uid = USER_ID * AID_USER_OFFSET + APPLICATION_ID;
    let invalid_access_vector = KeyPermission::CONVERT_STORAGE_KEY_TO_EPHEMERAL.0 << 19;

    let result = key_generations::map_ks_error(generate_ec_key_and_grant_to_user(
        grantee_uid.try_into().unwrap(),
        invalid_access_vector,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Rc(ResponseCode::SYSTEM_ERROR), result.unwrap_err());
}

/// Try to grant a key with empty access vector `KeyPermission::NONE`, should be able to grant a
/// key with empty access vector successfully. In grantee context try to use the granted key, it
/// should fail to load the key with permission denied error.
#[test]
fn keystore2_grant_key_with_perm_none() {
    static TARGET_SU_CTX: &str = "u:r:su:s0";

    static GRANTEE_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";
    const USER_ID: u32 = 99;
    const APPLICATION_ID: u32 = 10001;
    static GRANTEE_UID: u32 = USER_ID * AID_USER_OFFSET + APPLICATION_ID;
    static GRANTEE_GID: u32 = GRANTEE_UID;

    let grant_key_nspace = unsafe {
        run_as::run_as(TARGET_SU_CTX, Uid::from_raw(0), Gid::from_raw(0), || {
            let empty_access_vector = KeyPermission::NONE.0;

            let grant_key = key_generations::map_ks_error(generate_ec_key_and_grant_to_user(
                GRANTEE_UID.try_into().unwrap(),
                empty_access_vector,
            ))
            .unwrap();

            assert_eq!(grant_key.domain, Domain::GRANT);

            grant_key.nspace
        })
    };

    // In grantee context try to load the key, it should fail to load the granted key as it is
    // granted with empty access vector.
    unsafe {
        run_as::run_as(
            GRANTEE_CTX,
            Uid::from_raw(GRANTEE_UID),
            Gid::from_raw(GRANTEE_GID),
            move || {
                let keystore2 = get_keystore_service();

                let result = key_generations::map_ks_error(keystore2.getKeyEntry(&KeyDescriptor {
                    domain: Domain::GRANT,
                    nspace: grant_key_nspace,
                    alias: None,
                    blob: None,
                }));
                assert!(result.is_err());
                assert_eq!(Error::Rc(ResponseCode::PERMISSION_DENIED), result.unwrap_err());
            },
        )
    };
}

/// Grant a key to the user (grantee) with `GET_INFO|USE` key permissions. Verify whether grantee
/// can succeed in loading the granted key and try to perform simple operation using this granted
/// key. Grantee should be able to load the key and use the key to perform crypto operation
/// successfully. Try to delete the granted key in grantee context where it is expected to fail to
/// delete it as `DELETE` permission is not granted.
#[test]
fn keystore2_grant_get_info_use_key_perm() {
    static TARGET_SU_CTX: &str = "u:r:su:s0";

    static GRANTEE_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";
    const USER_ID: u32 = 99;
    const APPLICATION_ID: u32 = 10001;
    static GRANTEE_UID: u32 = USER_ID * AID_USER_OFFSET + APPLICATION_ID;
    static GRANTEE_GID: u32 = GRANTEE_UID;

    // Generate a key and grant it to a user with GET_INFO|USE key permissions.
    let grant_key_nspace = unsafe {
        run_as::run_as(TARGET_SU_CTX, Uid::from_raw(0), Gid::from_raw(0), || {
            let access_vector = KeyPermission::GET_INFO.0 | KeyPermission::USE.0;
            let grant_key = key_generations::map_ks_error(generate_ec_key_and_grant_to_user(
                GRANTEE_UID.try_into().unwrap(),
                access_vector,
            ))
            .unwrap();

            assert_eq!(grant_key.domain, Domain::GRANT);

            grant_key.nspace
        })
    };

    // In grantee context load the key and try to perform crypto operation.
    unsafe {
        run_as::run_as(
            GRANTEE_CTX,
            Uid::from_raw(GRANTEE_UID),
            Gid::from_raw(GRANTEE_GID),
            move || {
                let keystore2 = get_keystore_service();
                let sec_level =
                    keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

                // Load the granted key.
                let key_entry_response = keystore2
                    .getKeyEntry(&KeyDescriptor {
                        domain: Domain::GRANT,
                        nspace: grant_key_nspace,
                        alias: None,
                        blob: None,
                    })
                    .unwrap();

                // Perform sample crypto operation using granted key.
                let op_response = sec_level
                    .createOperation(
                        &key_entry_response.metadata.key,
                        &authorizations::AuthSetBuilder::new()
                            .purpose(KeyPurpose::SIGN)
                            .digest(Digest::SHA_2_256),
                        false,
                    )
                    .unwrap();
                assert!(op_response.iOperation.is_some());
                assert_eq!(
                    Ok(()),
                    key_generations::map_ks_error(perform_sample_sign_operation(
                        &op_response.iOperation.unwrap()
                    ))
                );

                // Try to delete the key, it is expected to be fail with permission denied error.
                let result = key_generations::map_ks_error(keystore2.deleteKey(&KeyDescriptor {
                    domain: Domain::GRANT,
                    nspace: grant_key_nspace,
                    alias: None,
                    blob: None,
                }));
                assert!(result.is_err());
                assert_eq!(Error::Rc(ResponseCode::PERMISSION_DENIED), result.unwrap_err());
            },
        )
    };
}

/// Try to generate a key with invalid Domain. `INVALID_ARGUMENT` error response is expected.
#[test]
fn keystore2_generate_key_invalid_domain() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("ks_invalid_test_key_{}", getuid());

    let result = key_generations::map_ks_error(key_generations::generate_ec_key(
        &*sec_level,
        Domain(99), // Invalid domain.
        key_generations::SELINUX_SHELL_NAMESPACE,
        Some(alias),
        EcCurve::P_256,
        Digest::SHA_2_256,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Rc(ResponseCode::INVALID_ARGUMENT), result.unwrap_err());
}

/// Try to generate a EC key without providing the curve.
/// `UNSUPPORTED_EC_CURVE or UNSUPPORTED_KEY_SIZE` error response is expected.
#[test]
fn keystore2_generate_ec_key_missing_curve() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("ks_ec_no_curve_test_key_{}", getuid());

    // Don't provide EC curve.
    let gen_params = authorizations::AuthSetBuilder::new()
        .no_auth_required()
        .algorithm(Algorithm::EC)
        .purpose(KeyPurpose::SIGN)
        .purpose(KeyPurpose::VERIFY)
        .digest(Digest::SHA_2_256);

    let result = key_generations::map_ks_error(sec_level.generateKey(
        &KeyDescriptor {
            domain: Domain::SELINUX,
            nspace: key_generations::SELINUX_SHELL_NAMESPACE,
            alias: Some(alias),
            blob: None,
        },
        None,
        &gen_params,
        0,
        b"entropy",
    ));
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(
        err,
        Error::Km(ErrorCode::UNSUPPORTED_EC_CURVE) | Error::Km(ErrorCode::UNSUPPORTED_KEY_SIZE)
    ));
}

/// Try to generate a EC key with curve `CURVE_25519` having `SIGN and AGREE_KEY` purposes.
/// `INCOMPATIBLE_PURPOSE` error response is expected.
#[test]
fn keystore2_generate_ec_key_25519_multi_purpose() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("ks_ec_no_curve_test_key_{}", getuid());

    // Specify `SIGN and AGREE_KEY` purposes.
    let gen_params = authorizations::AuthSetBuilder::new()
        .no_auth_required()
        .algorithm(Algorithm::EC)
        .ec_curve(EcCurve::CURVE_25519)
        .purpose(KeyPurpose::SIGN)
        .purpose(KeyPurpose::AGREE_KEY)
        .digest(Digest::SHA_2_256);

    let result = key_generations::map_ks_error(sec_level.generateKey(
        &KeyDescriptor {
            domain: Domain::SELINUX,
            nspace: key_generations::SELINUX_SHELL_NAMESPACE,
            alias: Some(alias),
            blob: None,
        },
        None,
        &gen_params,
        0,
        b"entropy",
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_PURPOSE), result.unwrap_err());
}

/// Generate EC keys with curves EcCurve::P_224, EcCurve::P_256, EcCurve::P_384, EcCurve::P_521 and
/// various digest modes. Try to create operations using generated keys. Operations with digest
/// modes `SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512` should be created  successfully.
/// Creation of operations with digest modes NONE and MD5 should fail with an error code
/// `UNSUPPORTED_DIGEST`.
#[test]
fn keystore2_ec_generate_key() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::NONE,
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let ec_curves = [EcCurve::P_224, EcCurve::P_256, EcCurve::P_384, EcCurve::P_521];

    for ec_curve in ec_curves {
        for digest in digests {
            let alias = format!("ks_ec_test_key_gen_{}{}{}", getuid(), ec_curve.0, digest.0);
            let key_metadata = key_generations::generate_ec_key(
                &*sec_level,
                Domain::APP,
                -1,
                Some(alias.to_string()),
                ec_curve,
                digest,
            )
            .unwrap();

            match key_generations::map_ks_error(sec_level.createOperation(
                &key_metadata.key,
                &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(digest),
                false,
            )) {
                Ok(op_response) => {
                    assert!(op_response.iOperation.is_some());
                    assert_eq!(
                        Ok(()),
                        key_generations::map_ks_error(perform_sample_sign_operation(
                            &op_response.iOperation.unwrap()
                        ))
                    );
                }
                Err(e) => {
                    assert_eq!(e, Error::Km(ErrorCode::UNSUPPORTED_DIGEST));
                    assert!(digest == Digest::NONE || digest == Digest::MD5);
                }
            }
        }
    }
}

/// Generate EC key with curve `CURVE_25519` and digest mode NONE. Try to create an operation using
/// generated key. `CURVE_25519` key should support `Digest::NONE` digest mode and test should be
/// able to create an operation successfully.
#[test]
fn keystore2_ec_25519_generate_key_success() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = format!("ks_ec_25519_none_test_key_gen_{}", getuid());
    let key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias),
        EcCurve::CURVE_25519,
        Digest::NONE,
    )
    .unwrap();

    let op_response = sec_level
        .createOperation(
            &key_metadata.key,
            &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(Digest::NONE),
            false,
        )
        .unwrap();
    assert!(op_response.iOperation.is_some());
    assert_eq!(
        Ok(()),
        key_generations::map_ks_error(perform_sample_sign_operation(
            &op_response.iOperation.unwrap()
        ))
    );
}

/// Generate EC keys with curve `CURVE_25519` and digest modes `MD5, SHA1, SHA-2 224, SHA-2 256,
/// SHA-2 384 and SHA-2 512`. Try to create operations using generated keys. `CURVE_25519` keys
/// shouldn't support these digest modes. Test should fail to create operations with an error
/// `UNSUPPORTED_DIGEST`.
#[test]
fn keystore2_ec_25519_generate_key_fail() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    for digest in digests {
        let alias = format!("ks_ec_25519_test_key_gen_{}{}", getuid(), digest.0);
        let key_metadata = key_generations::generate_ec_key(
            &*sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            EcCurve::CURVE_25519,
            digest,
        )
        .unwrap();

        let result = key_generations::map_ks_error(sec_level.createOperation(
            &key_metadata.key,
            &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(digest),
            false,
        ));
        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_DIGEST), result.unwrap_err());
    }
}

/// Generate a EC key with `SHA_2_256` digest mode. Try to create an operation with digest mode
/// other than `SHA_2_256`. Creation of an operation with generated key should fail with
/// `INCOMPATIBLE_DIGEST` error as there is a mismatch of digest mode in key authorizations.
#[test]
fn keystore2_create_op_with_incompatible_key_digest() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_ec_test_incomp_key_digest";
    let key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .unwrap();

    let digests =
        [Digest::NONE, Digest::SHA1, Digest::SHA_2_224, Digest::SHA_2_384, Digest::SHA_2_512];

    for digest in digests {
        let result = key_generations::map_ks_error(sec_level.createOperation(
            &key_metadata.key,
            &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(digest),
            false,
        ));
        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_DIGEST), result.unwrap_err());
    }
}

/// Generate a key in client#1 and try to use it in other client#2.
/// Client#2 should fail to load the key as the it doesn't own the client#1 generated key.
#[test]
fn keystore2_key_owner_validation() {
    static TARGET_CTX: &str = "u:r:untrusted_app:s0:c91,c256,c10,c20";
    const USER_ID: u32 = 99;
    const APPLICATION_ID_1: u32 = 10601;

    let uid1 = USER_ID * AID_USER_OFFSET + APPLICATION_ID_1;
    let gid1 = USER_ID * AID_USER_OFFSET + APPLICATION_ID_1;
    let alias = "ks_owner_check_test_key";

    // Client#1: Generate a key and create an operation using generated key.
    // Wait until the parent notifies to continue. Once the parent notifies, this operation
    // is expected to be completed successfully.
    let mut child_handle = execute_op_run_as_child(
        TARGET_CTX,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        Uid::from_raw(uid1),
        Gid::from_raw(gid1),
        ForcedOp(false),
    );

    // Wait until (client#1) child process notifies us to continue, so that there will be a key
    // generated by client#1.
    child_handle.recv();

    // Client#2: This child will try to load the key generated by client#1.
    const APPLICATION_ID_2: u32 = 10602;
    let uid2 = USER_ID * AID_USER_OFFSET + APPLICATION_ID_2;
    let gid2 = USER_ID * AID_USER_OFFSET + APPLICATION_ID_2;
    unsafe {
        run_as::run_as(TARGET_CTX, Uid::from_raw(uid2), Gid::from_raw(gid2), move || {
            let keystore2_inst = get_keystore_service();
            let result =
                key_generations::map_ks_error(keystore2_inst.getKeyEntry(&KeyDescriptor {
                    domain: Domain::APP,
                    nspace: -1,
                    alias: Some(alias.to_string()),
                    blob: None,
                }));
            assert!(result.is_err());
            assert_eq!(Error::Rc(ResponseCode::KEY_NOT_FOUND), result.unwrap_err());
        });
    };

    // Notify the child process (client#1) to resume and finish.
    child_handle.send(&BarrierReached {});
    assert!(
        (child_handle.get_result() == TestOutcome::Ok),
        "Client#1 failed to complete the operation."
    );
}

/// Generate EC key with BLOB as domain. Generated key should be returned to caller as key blob.
/// Verify that `blob` field in the `KeyDescriptor` is not empty and should have the key blob.
/// Try to use this key for performing a sample operation and the operation should complete
/// successfully.
#[test]
fn keystore2_generate_key_with_blob_domain() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::BLOB,
        key_generations::SELINUX_SHELL_NAMESPACE,
        None,
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .unwrap();

    assert!(key_metadata.certificate.is_some());
    assert!(key_metadata.certificateChain.is_none());

    // Must have the key blob.
    assert!(key_metadata.key.blob.is_some());

    let op_response = key_generations::map_ks_error(sec_level.createOperation(
        &key_metadata.key,
        &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(Digest::SHA_2_256),
        false,
    ))
    .unwrap();
    assert!(op_response.iOperation.is_some());
    assert_eq!(
        Ok(()),
        key_generations::map_ks_error(perform_sample_sign_operation(
            &op_response.iOperation.unwrap()
        ))
    );

    // Delete the generated key blob.
    sec_level.deleteKey(&key_metadata.key).unwrap();
}

/// Try to generate a key with `Domain::KEY_ID`, test should fail with an error code
/// `SYSTEM_ERROR`. `Domain::KEY_ID` is not allowed to use for generating a key. Key id is returned
/// by Keystore2 after a key has been mapped from an alias.
#[test]
fn keystore2_generate_key_with_key_id_domain_expect_sys_error() {
    let alias = "ks_gen_key_id_test_key";
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let result = key_generations::map_ks_error(key_generations::generate_ec_key(
        &*sec_level,
        Domain::KEY_ID,
        key_generations::SELINUX_SHELL_NAMESPACE,
        Some(alias.to_string()),
        EcCurve::P_256,
        Digest::SHA_2_256,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Rc(ResponseCode::SYSTEM_ERROR), result.unwrap_err());
}

/// Generate a key and try to load the generated key using KEY_ID as domain. Create an
/// operation using key which is loaded with domain as KEY_ID. Test should create an operation
/// successfully.
#[test]
fn keystore2_find_key_with_key_id_as_domain() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "ks_key_id_test_key";

    let key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .expect("Failed to generate a EC key.");

    // Try to load the above generated key with KEY_ID as domain.
    let key_entry_response = keystore2
        .getKeyEntry(&KeyDescriptor {
            domain: Domain::KEY_ID,
            nspace: key_metadata.key.nspace,
            alias: Some(alias.to_string()),
            blob: None,
        })
        .expect("Error in getKeyEntry to load a key with domain KEY_ID.");

    // Verify above found key is same the one generated.
    assert_eq!(key_metadata.key, key_entry_response.metadata.key);
    assert_eq!(key_metadata.certificate, key_entry_response.metadata.certificate);
    assert_eq!(key_metadata.certificateChain, key_entry_response.metadata.certificateChain);
    assert_eq!(key_metadata.key.nspace, key_entry_response.metadata.key.nspace);

    // Try to create an operation using above loaded key, operation should be created
    // successfully.
    let op_response = sec_level
        .createOperation(
            &key_entry_response.metadata.key,
            &authorizations::AuthSetBuilder::new()
                .purpose(KeyPurpose::SIGN)
                .digest(Digest::SHA_2_256),
            false,
        )
        .expect("Error in creation of operation.");

    assert!(op_response.iOperation.is_some());
    assert_eq!(
        Ok(()),
        key_generations::map_ks_error(perform_sample_sign_operation(
            &op_response.iOperation.unwrap()
        ))
    );
}

/// Generate a key with an alias. Generate another key and bind it to the same alias.
/// Try to create an operation using previously generated key. Creation of an operation should
/// fail because previously generated key material is no longer accessible. Test should successfully
/// create an operation using the rebound key.
#[test]
fn keystore2_key_id_alias_rebind_verify_by_alias() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("ks_key_id_test_alias_rebind_1_{}", getuid());

    let key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .expect("Failed to generate a EC key.");

    // Generate a key with same alias as above generated key, so that alias will be rebound
    // to this key.
    let new_key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias),
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .expect("Failed to generate a rebound EC key.");

    assert_ne!(key_metadata.key, new_key_metadata.key);
    assert_ne!(key_metadata.certificate, new_key_metadata.certificate);
    assert_ne!(key_metadata.key.nspace, new_key_metadata.key.nspace);

    // Try to create an operation using previously generated key_metadata.
    // It should fail as previously generated key material is no longer remains valid.
    let result = key_generations::map_ks_error(sec_level.createOperation(
        &key_metadata.key,
        &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(Digest::SHA_2_256),
        false,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Rc(ResponseCode::KEY_NOT_FOUND), result.unwrap_err());

    // Try to create an operation using rebound key, operation should be created
    // successfully.
    let op_response = sec_level
        .createOperation(
            &new_key_metadata.key,
            &authorizations::AuthSetBuilder::new()
                .purpose(KeyPurpose::SIGN)
                .digest(Digest::SHA_2_256),
            false,
        )
        .expect("Error in creation of operation using rebound key.");

    assert!(op_response.iOperation.is_some());
    assert_eq!(
        Ok(()),
        key_generations::map_ks_error(perform_sample_sign_operation(
            &op_response.iOperation.unwrap()
        ))
    );
}

/// Generate a key with an alias. Load the generated key with `Domain::KEY_ID`. Generate another
/// key and bind it to the same alias. Try to create an operation using the key loaded with domain
/// `KEY_ID`. Creation of an operation should fail because originally loaded key no longer exists.
/// Test should successfully create an operation using the rebound key.
#[test]
fn keystore2_key_id_alias_rebind_verify_by_key_id() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = format!("ks_key_id_test_alias_rebind_2_{}", getuid());

    let key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .expect("Failed to generate a EC key.");

    // Load the above generated key with KEY_ID as domain.
    let key_entry_response = keystore2
        .getKeyEntry(&KeyDescriptor {
            domain: Domain::KEY_ID,
            nspace: key_metadata.key.nspace,
            alias: Some(alias.to_string()),
            blob: None,
        })
        .expect("Error in getKeyEntry to load a key with domain KEY_ID.");

    // Verify above found key is same the one generated.
    assert_eq!(key_metadata.key, key_entry_response.metadata.key);
    assert_eq!(key_metadata.certificate, key_entry_response.metadata.certificate);
    assert_eq!(key_metadata.certificateChain, key_entry_response.metadata.certificateChain);
    assert_eq!(key_metadata.key.nspace, key_entry_response.metadata.key.nspace);

    // Generate another key with same alias as above generated key, so that alias will be rebound
    // to this key.
    let new_key_metadata = key_generations::generate_ec_key(
        &*sec_level,
        Domain::APP,
        -1,
        Some(alias),
        EcCurve::P_256,
        Digest::SHA_2_256,
    )
    .expect("Failed to generate a rebound EC key.");

    // Verify that an alias is rebound to a new key.
    assert_eq!(key_metadata.key.alias, new_key_metadata.key.alias);
    assert_ne!(key_metadata.key, new_key_metadata.key);
    assert_ne!(key_metadata.certificate, new_key_metadata.certificate);
    assert_ne!(key_metadata.key.nspace, new_key_metadata.key.nspace);

    // Try to create an operation using previously loaded key_entry_response.
    // It should fail as previously generated key material is no longer valid.
    let result = key_generations::map_ks_error(sec_level.createOperation(
        &key_entry_response.metadata.key,
        &authorizations::AuthSetBuilder::new().purpose(KeyPurpose::SIGN).digest(Digest::SHA_2_256),
        false,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Rc(ResponseCode::KEY_NOT_FOUND), result.unwrap_err());

    // Try to create an operation using rebound key, operation should be created
    // successfully.
    let op_response = sec_level
        .createOperation(
            &new_key_metadata.key,
            &authorizations::AuthSetBuilder::new()
                .purpose(KeyPurpose::SIGN)
                .digest(Digest::SHA_2_256),
            false,
        )
        .expect("Error in creation of operation using rebound key.");

    assert!(op_response.iOperation.is_some());
    assert_eq!(
        Ok(()),
        key_generations::map_ks_error(perform_sample_sign_operation(
            &op_response.iOperation.unwrap()
        ))
    );
}

/// Generate RSA signing keys with -
///     Padding mode: RSA_PKCS1_1_5_SIGN
///     Digest modes: `NONE, MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
/// Create operations with these generated keys. Test should create operations successfully.
#[test]
fn keystore2_rsa_generate_signing_key_padding_pkcs1_1_5() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::NONE,
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for digest in digests {
            let alias = format!("ks_rsa_key_test_{}{}{}", getuid(), key_size, digest.0);
            let op_response = create_rsa_key_and_operation(
                &sec_level,
                Domain::APP,
                -1,
                Some(alias.to_string()),
                &key_generations::KeyParams {
                    key_size,
                    purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
                    padding: Some(PaddingMode::RSA_PKCS1_1_5_SIGN),
                    digest: Some(digest),
                    mgf_digest: None,
                    block_mode: None,
                    att_challenge: None,
                    att_app_id: None,
                },
                KeyPurpose::SIGN,
                ForcedOp(false),
            )
            .unwrap();

            assert!(op_response.iOperation.is_some());
            assert_eq!(
                Ok(()),
                key_generations::map_ks_error(perform_sample_sign_operation(
                    &op_response.iOperation.unwrap()
                ))
            );
        } // End of digests.
    } // End of key-sizes.
}

/// Generate RSA signing keys with -
///     Padding mode: RSA_PSS
///     Digest modes: `MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
/// Create operations with these generated keys. Test should create operations successfully.
#[test]
fn keystore2_rsa_generate_signing_key_padding_pss_success() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for digest in digests {
            let alias = format!("ks_rsa_key_test_{}{}{}", getuid(), key_size, digest.0);
            let op_response = create_rsa_key_and_operation(
                &sec_level,
                Domain::APP,
                -1,
                Some(alias.to_string()),
                &key_generations::KeyParams {
                    key_size,
                    purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
                    padding: Some(PaddingMode::RSA_PSS),
                    digest: Some(digest),
                    mgf_digest: None,
                    block_mode: None,
                    att_challenge: None,
                    att_app_id: None,
                },
                KeyPurpose::SIGN,
                ForcedOp(false),
            )
            .unwrap();

            assert!(op_response.iOperation.is_some());
            assert_eq!(
                Ok(()),
                key_generations::map_ks_error(perform_sample_sign_operation(
                    &op_response.iOperation.unwrap()
                ))
            );
        } // End of digests.
    } // End of key-sizes.
}

/// Generate RSA signing key with -
///     Padding mode: RSA_PSS
///     Digest mode: `NONE`.
/// Try to create an operation with this generated key. Test should fail to create an operation with
/// `INCOMPATIBLE_DIGEST` error code.
#[test]
fn keystore2_rsa_generate_signing_key_padding_pss_fail() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        let alias = format!("ks_rsa_pss_none_key_test_{}{}", getuid(), key_size);
        let result = key_generations::map_ks_error(create_rsa_key_and_operation(
            &sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            &key_generations::KeyParams {
                key_size,
                purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
                padding: Some(PaddingMode::RSA_PSS),
                digest: Some(Digest::NONE),
                mgf_digest: None,
                block_mode: None,
                att_challenge: None,
                att_app_id: None,
            },
            KeyPurpose::SIGN,
            ForcedOp(false),
        ));
        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_DIGEST), result.unwrap_err());
    }
}

/// Generate RSA signing key with -
///     Padding mode: `NONE`
///     Digest mode `NONE`
/// Try to create an operation with this generated key. Test should create an operation successfully.
#[test]
fn keystore2_rsa_generate_signing_key_padding_none_success() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        let alias = format!("ks_rsa_pad_none_key_test_{}{}", getuid(), key_size);
        let op_response = create_rsa_key_and_operation(
            &sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            &key_generations::KeyParams {
                key_size,
                purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
                padding: Some(PaddingMode::NONE),
                digest: Some(Digest::NONE),
                mgf_digest: None,
                block_mode: None,
                att_challenge: None,
                att_app_id: None,
            },
            KeyPurpose::SIGN,
            ForcedOp(false),
        )
        .unwrap();

        assert!(op_response.iOperation.is_some());
        assert_eq!(
            Ok(()),
            key_generations::map_ks_error(perform_sample_sign_operation(
                &op_response.iOperation.unwrap()
            ))
        );
    }
}

/// Generate RSA signing keys with -
///     Padding mode: `NONE`
///     Digest modes: `MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
/// Create operations with these generated keys. Test should fail to create operations with
/// an error code `UNKNOWN_ERROR`.
#[test]
fn keystore2_rsa_generate_signing_key_padding_none_fail() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for digest in digests {
            let alias = format!("ks_rsa_key_test_{}{}{}", getuid(), key_size, digest.0);
            let result = key_generations::map_ks_error(create_rsa_key_and_operation(
                &sec_level,
                Domain::APP,
                -1,
                Some(alias.to_string()),
                &key_generations::KeyParams {
                    key_size,
                    purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
                    padding: Some(PaddingMode::NONE),
                    digest: Some(digest),
                    mgf_digest: None,
                    block_mode: None,
                    att_challenge: None,
                    att_app_id: None,
                },
                KeyPurpose::SIGN,
                ForcedOp(false),
            ));
            assert!(result.is_err());
            assert_eq!(Error::Km(ErrorCode::UNKNOWN_ERROR), result.unwrap_err());
        }
    }
}

/// Generate RSA keys with -
///     Padding Mode: `RSA_OAEP`
///     Digest modes: `MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
///     mgf-digests: `MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
/// Create a decrypt operations using generated keys. Test should create operations successfully.
#[test]
fn keystore2_rsa_generate_key_with_oaep_padding_success() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let mgf_digests = [
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for digest in digests {
            for mgf_digest in mgf_digests {
                let alias =
                    format!("ks_rsa_key_pair_oaep_test_{}{}{}", getuid(), key_size, digest.0);
                let result = create_rsa_key_and_operation(
                    &sec_level,
                    Domain::APP,
                    -1,
                    Some(alias.to_string()),
                    &key_generations::KeyParams {
                        key_size,
                        purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                        padding: Some(PaddingMode::RSA_OAEP),
                        digest: Some(digest),
                        mgf_digest: Some(mgf_digest),
                        block_mode: Some(BlockMode::ECB),
                        att_challenge: None,
                        att_app_id: None,
                    },
                    KeyPurpose::DECRYPT,
                    ForcedOp(false),
                );
                assert!(result.is_ok());
            } // End of mgf-digests.
        } // End of digests.
    } // End of key-sizes.
}

/// Generate RSA keys with -
///     Padding mode: `RSA_OAEP`
///     Digest modes: `MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
/// Create a decrypt operations using generated keys. Test should create operations successfully.
#[test]
fn keystore2_rsa_generate_key_with_oaep_padding_and_digests_success() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for digest in digests {
            let alias = format!("ks_rsa_key_pair_oaep_test_{}{}{}", getuid(), key_size, digest.0);
            let result = create_rsa_key_and_operation(
                &sec_level,
                Domain::APP,
                -1,
                Some(alias.to_string()),
                &key_generations::KeyParams {
                    key_size,
                    purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                    padding: Some(PaddingMode::RSA_OAEP),
                    digest: Some(digest),
                    mgf_digest: None,
                    block_mode: Some(BlockMode::ECB),
                    att_challenge: None,
                    att_app_id: None,
                },
                KeyPurpose::DECRYPT,
                ForcedOp(false),
            );
            assert!(result.is_ok());
        } // End of digests.
    } // End of key-sizes.
}

/// Generate RSA encryption key with -
///     Digest mode: `NONE`
///     Padding mode: `RSA_OAEP`
/// Try to create an operation using generated key. Test should fail to create an operation
/// with an error code `INCOMPATIBLE_DIGEST`.
#[test]
fn keystore2_rsa_generate_key_with_oaep_padding_fail() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        let alias = format!("ks_rsa_key_padding_{}{}", getuid(), key_size);
        let result = key_generations::map_ks_error(create_rsa_key_and_operation(
            &sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            &key_generations::KeyParams {
                key_size,
                purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                padding: Some(PaddingMode::RSA_OAEP),
                digest: Some(Digest::NONE),
                mgf_digest: None,
                block_mode: None,
                att_challenge: None,
                att_app_id: None,
            },
            KeyPurpose::DECRYPT,
            ForcedOp(false),
        ));

        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_DIGEST), result.unwrap_err());
    }
}

/// Generate RSA encryption keys with various digest mode and padding mode combinations.
///     Digest modes: `MD5, SHA1, SHA-2 224, SHA-2 256, SHA-2 384 and SHA-2 512`
///     Padding modes: `NONE, RSA_PKCS1_1_5_ENCRYPT`
/// Try to create operations using generated keys, test should create operations successfully.
#[test]
fn keystore2_rsa_generate_keys_with_digest_paddings() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let digests = [
        Digest::NONE,
        Digest::MD5,
        Digest::SHA1,
        Digest::SHA_2_224,
        Digest::SHA_2_256,
        Digest::SHA_2_384,
        Digest::SHA_2_512,
    ];

    let paddings = [PaddingMode::NONE, PaddingMode::RSA_PKCS1_1_5_ENCRYPT];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for digest in digests {
            for padding in paddings {
                let alias = format!("ks_rsa_key_padding_{}{}{}", getuid(), key_size, digest.0);
                let result = key_generations::map_ks_error(create_rsa_key_and_operation(
                    &sec_level,
                    Domain::APP,
                    -1,
                    Some(alias.to_string()),
                    &key_generations::KeyParams {
                        key_size,
                        purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                        padding: Some(padding),
                        digest: Some(digest),
                        mgf_digest: None,
                        block_mode: None,
                        att_challenge: None,
                        att_app_id: None,
                    },
                    KeyPurpose::DECRYPT,
                    ForcedOp(false),
                ));

                assert!(result.is_ok());
            } // End of paddings.
        } // End of digests.
    } // End of key-sizes.
}

/// Generate RSA encryption keys with only padding modes.
///     Padding modes: `NONE, RSA_PKCS1_1_5_ENCRYPT`
/// Try to create operations using generated keys, test should create operations successfully.
#[test]
fn keystore2_rsa_generate_keys_with_paddings() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let paddings = [PaddingMode::NONE, PaddingMode::RSA_PKCS1_1_5_ENCRYPT];

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        for padding in paddings {
            let alias = format!("ks_rsa_key_padding_{}{}", getuid(), key_size);
            let result = create_rsa_key_and_operation(
                &sec_level,
                Domain::APP,
                -1,
                Some(alias.to_string()),
                &key_generations::KeyParams {
                    key_size,
                    purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                    padding: Some(padding),
                    digest: None,
                    mgf_digest: None,
                    block_mode: None,
                    att_challenge: None,
                    att_app_id: None,
                },
                KeyPurpose::DECRYPT,
                ForcedOp(false),
            );
            assert!(result.is_ok());
        } // End of paddings.
    } // End of key-sizes.
}

/// Generate RSA keys without padding and digest modes. Try to create decrypt operation without
/// digest and padding. Creation of an operation should fail with an error code
/// `UNSUPPORTED_PADDING_MODE`.
#[test]
fn keystore2_rsa_generate_keys() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let key_sizes = [2048, 3072, 4096];

    for key_size in key_sizes {
        let alias = format!("ks_rsa_key_test_{}{}", getuid(), key_size);
        let result = key_generations::map_ks_error(create_rsa_key_and_operation(
            &sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            &key_generations::KeyParams {
                key_size,
                purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                padding: None,
                digest: None,
                mgf_digest: None,
                block_mode: None,
                att_challenge: None,
                att_app_id: None,
            },
            KeyPurpose::DECRYPT,
            ForcedOp(false),
        ));
        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_PADDING_MODE), result.unwrap_err());
    }
}

/// Generate a RSA encryption key. Try to create a signing operation with it, an error
/// `INCOMPATIBLE_PURPOSE` is expected as the generated key doesn't support sign operation.
#[test]
fn keystore2_rsa_encrypt_key_op_invalid_purpose() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_test_key_1";
    let result = key_generations::map_ks_error(create_rsa_key_and_operation(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 2048,
            purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
            padding: Some(PaddingMode::RSA_PKCS1_1_5_ENCRYPT),
            digest: Some(Digest::SHA_2_256),
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        KeyPurpose::SIGN,
        ForcedOp(false),
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_PURPOSE), result.unwrap_err());
}

/// Generate a RSA signing key. Try to create a decrypt operation with it, an error
/// `INCOMPATIBLE_PURPOSE` is expected as the generated key doesn't support decrypt operation.
#[test]
fn keystore2_rsa_sign_key_op_invalid_purpose() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_test_key_2";
    let result = key_generations::map_ks_error(create_rsa_key_and_operation(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 2048,
            purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
            padding: Some(PaddingMode::RSA_PKCS1_1_5_SIGN),
            digest: Some(Digest::SHA_2_256),
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        KeyPurpose::DECRYPT,
        ForcedOp(false),
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_PURPOSE), result.unwrap_err());
}

/// Generate a RSA key with SIGN and AGREE_KEY purposes. Try to perform an operation using the
/// generated key, an error `UNSUPPORTED_PURPOSE` is expected as RSA doesn't support AGREE_KEY.
#[test]
fn keystore2_rsa_key_unsupported_purpose() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_key_test_3";
    let result = key_generations::map_ks_error(create_rsa_key_and_operation(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 2048,
            purpose: vec![KeyPurpose::AGREE_KEY],
            padding: Some(PaddingMode::RSA_PKCS1_1_5_SIGN),
            digest: Some(Digest::SHA_2_256),
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        KeyPurpose::AGREE_KEY,
        ForcedOp(false),
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_PURPOSE), result.unwrap_err());
}

/// Generate a RSA encrypt key with padding mode supported for signing. Try to create an operation
/// using generated key, an error `UNSUPPORTED_PADDING_MODE` is expected with unsupported padding
/// mode.
#[test]
fn keystore2_rsa_encrypt_key_unsupported_padding() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let paddings = [PaddingMode::RSA_PKCS1_1_5_SIGN, PaddingMode::RSA_PSS];

    for padding in paddings {
        let alias = format!("ks_rsa_key_test_4_{}{}", getuid(), padding.0);
        let result = key_generations::map_ks_error(create_rsa_key_and_operation(
            &sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            &key_generations::KeyParams {
                key_size: 2048,
                purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
                padding: Some(padding),
                digest: Some(Digest::SHA_2_256),
                mgf_digest: None,
                block_mode: None,
                att_challenge: None,
                att_app_id: None,
            },
            KeyPurpose::DECRYPT,
            ForcedOp(false),
        ));
        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_PADDING_MODE), result.unwrap_err());
    }
}

/// Generate a RSA signing key with padding mode supported for encryption. Try to create an
/// operation using generated key, an error `UNSUPPORTED_PADDING_MODE` is expected with
/// unsupported padding mode.
#[test]
fn keystore2_rsa_signing_key_unsupported_padding() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let paddings = [PaddingMode::RSA_PKCS1_1_5_ENCRYPT, PaddingMode::RSA_OAEP];

    for padding in paddings {
        let alias = format!("ks_rsa_key_test_4_{}{}", getuid(), padding.0);
        let result = key_generations::map_ks_error(create_rsa_key_and_operation(
            &sec_level,
            Domain::APP,
            -1,
            Some(alias.to_string()),
            &key_generations::KeyParams {
                key_size: 2048,
                purpose: vec![KeyPurpose::SIGN, KeyPurpose::VERIFY],
                padding: Some(padding),
                digest: Some(Digest::SHA_2_256),
                mgf_digest: None,
                block_mode: None,
                att_challenge: None,
                att_app_id: None,
            },
            KeyPurpose::SIGN,
            ForcedOp(false),
        ));
        assert!(result.is_err());
        assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_PADDING_MODE), result.unwrap_err());
    }
}

/// Generate a RSA encryption key. Try to perform encrypt operation using the generated
/// key, an error `UNSUPPORTED_PURPOSE` is expected as encrypt operation is not supported
/// with RSA key.
#[test]
fn keystore2_rsa_key_unsupported_op() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_key_test_5";
    let result = key_generations::map_ks_error(create_rsa_key_and_operation(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 2048,
            purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
            padding: Some(PaddingMode::RSA_PKCS1_1_5_ENCRYPT),
            digest: Some(Digest::SHA_2_256),
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        KeyPurpose::ENCRYPT,
        ForcedOp(false),
    ));

    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_PURPOSE), result.unwrap_err());
}

/// Generate a RSA key with encrypt, sign and verify purpose. Try to perform decrypt operation
/// using the generated key, an error `INCOMPATIBLE_PURPOSE` is expected as the key is not
/// generated with decrypt purpose.
#[test]
fn keystore2_rsa_key_missing_purpose() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_key_test_6";
    let result = key_generations::map_ks_error(create_rsa_key_and_operation(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 2048,
            purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::SIGN, KeyPurpose::VERIFY],
            padding: Some(PaddingMode::RSA_PKCS1_1_5_ENCRYPT),
            digest: Some(Digest::SHA_2_256),
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        KeyPurpose::DECRYPT,
        ForcedOp(false),
    ));

    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_PURPOSE), result.unwrap_err());
}

/// Generate RSA encryption keys with OAEP padding mode and without digest mode. Try to create an
/// operation with generated key, unsupported digest error is expected.
#[test]
fn keystore2_rsa_gen_keys_with_oaep_paddings_without_digest() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_key_padding_fail";
    let result = key_generations::map_ks_error(create_rsa_key_and_operation(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 2048,
            purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::DECRYPT],
            padding: Some(PaddingMode::RSA_OAEP),
            digest: None,
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        KeyPurpose::DECRYPT,
        ForcedOp(false),
    ));

    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_DIGEST), result.unwrap_err());
}

/// Generate RSA keys with unsupported key size, an error `UNSUPPORTED_KEY_SIZE` is expected.
#[test]
fn keystore2_rsa_gen_keys_unsupported_size() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let alias = "ks_rsa_key_padding_fail";
    let result = key_generations::map_ks_error(key_generations::generate_rsa_key(
        &sec_level,
        Domain::APP,
        -1,
        Some(alias.to_string()),
        &key_generations::KeyParams {
            key_size: 5120,
            purpose: vec![KeyPurpose::ENCRYPT, KeyPurpose::SIGN, KeyPurpose::VERIFY],
            padding: Some(PaddingMode::RSA_PKCS1_1_5_ENCRYPT),
            digest: Some(Digest::SHA_2_256),
            mgf_digest: None,
            block_mode: None,
            att_challenge: None,
            att_app_id: None,
        },
        None,
    ));

    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_KEY_SIZE), result.unwrap_err());
}

/// Generate AES keys with various block modes and paddings.
///  - Block Modes: ECB, CBC
///  - Padding Modes: NONE, PKCS7
/// Test should generate keys and perform operation successfully.
#[test]
fn keystore2_aes_ecb_cbc_generate_key() {
    let keystore2 = get_keystore_service();
    let key_sizes = [128, 256];
    let block_modes = [BlockMode::ECB, BlockMode::CBC];
    let padding_modes = [PaddingMode::PKCS7, PaddingMode::NONE];

    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    for key_size in key_sizes {
        for block_mode in block_modes {
            for padding_mode in padding_modes {
                assert_eq!(
                    Ok(()),
                    create_aes_key_and_operation(
                        &sec_level,
                        key_size,
                        padding_mode,
                        block_mode,
                        None,
                        None,
                        &mut None,
                    )
                );
            }
        }
    }
}

/// Generate AES keys with -
///  - Block Modes: `CTR, GCM`
///  - Padding Modes: `NONE`
/// Test should generate keys and perform operation successfully.
#[test]
fn keystore2_aes_ctr_gcm_generate_key_success() {
    let keystore2 = get_keystore_service();
    let key_sizes = [128, 256];
    let key_params = [(BlockMode::CTR, None, None), (BlockMode::GCM, Some(128), Some(128))];

    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    for key_size in key_sizes {
        for (block_mode, mac_len, min_mac_len) in key_params {
            let result = key_generations::map_ks_error(create_aes_key_and_operation(
                &sec_level,
                key_size,
                PaddingMode::NONE,
                block_mode,
                mac_len,
                min_mac_len,
                &mut None,
            ));

            assert_eq!(Ok(()), result);
        } // End of block mode.
    } // End of key size.
}

/// Generate AES keys with -
///  - Block Modes: `CTR, GCM`
///  - Padding Modes: `PKCS7`
/// Try to create an operation using generated keys, test should fail to create an operation
/// with an error code `INCOMPATIBLE_PADDING_MODE`.
#[test]
fn keystore2_aes_ctr_gcm_generate_key_fails_incompatible() {
    let keystore2 = get_keystore_service();
    let key_sizes = [128, 256];
    let key_params = [(BlockMode::CTR, None, None), (BlockMode::GCM, Some(128), Some(128))];

    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    for key_size in key_sizes {
        for (block_mode, mac_len, min_mac_len) in key_params {
            let result = key_generations::map_ks_error(create_aes_key_and_operation(
                &sec_level,
                key_size,
                PaddingMode::PKCS7,
                block_mode,
                mac_len,
                min_mac_len,
                &mut None,
            ));

            assert!(result.is_err());
            assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_PADDING_MODE), result.unwrap_err());
        } // End of block mode.
    } // End of key size.
}

/// Try to generate AES key with invalid key size. Test should fail to generate a key with
/// an error code `UNSUPPORTED_KEY_SIZE`.
#[test]
fn keystore2_aes_key_fails_unsupported_key_size() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_invalid_1";

    let result = key_generations::map_ks_error(key_generations::generate_aes_key(
        &sec_level,
        1024,
        alias,
        &PaddingMode::NONE,
        &BlockMode::ECB,
        None,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_KEY_SIZE), result.unwrap_err());
}

/// Try to generate AES key with GCM block mode without providing `MIN_MAC_LENGTH`.
/// Test should fail to generate a key with an error code `MISSING_MIN_MAC_LENGTH`.
#[test]
fn keystore2_aes_gcm_key_fails_missing_min_mac_len() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_invalid_1";

    let result = key_generations::map_ks_error(key_generations::generate_aes_key(
        &sec_level,
        128,
        alias,
        &PaddingMode::NONE,
        &BlockMode::GCM,
        None,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::MISSING_MIN_MAC_LENGTH), result.unwrap_err());
}

/// Try to create an operation using AES key with multiple block modes. Test should fail to create
/// an operation with `UNSUPPORTED_BLOCK_MODE` error code.
#[test]
fn keystore2_aes_key_op_fails_multi_block_modes() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_invalid_1";

    let gen_params = authorizations::AuthSetBuilder::new()
        .no_auth_required()
        .algorithm(Algorithm::AES)
        .purpose(KeyPurpose::ENCRYPT)
        .purpose(KeyPurpose::DECRYPT)
        .key_size(128)
        .block_mode(BlockMode::ECB)
        .block_mode(BlockMode::CBC)
        .padding_mode(PaddingMode::NONE);

    let key_metadata = sec_level
        .generateKey(
            &KeyDescriptor {
                domain: Domain::APP,
                nspace: -1,
                alias: Some(alias.to_string()),
                blob: None,
            },
            None,
            &gen_params,
            0,
            b"entropy",
        )
        .unwrap();

    let op_params = authorizations::AuthSetBuilder::new()
        .purpose(KeyPurpose::ENCRYPT)
        .block_mode(BlockMode::ECB)
        .block_mode(BlockMode::CBC)
        .padding_mode(PaddingMode::NONE);

    let result = key_generations::map_ks_error(sec_level.createOperation(
        &key_metadata.key,
        &op_params,
        false,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_BLOCK_MODE), result.unwrap_err());
}

/// Try to create an operation using AES key with multiple padding modes. Test should fail to create
/// an operation with `UNSUPPORTED_PADDING_MODE` error code.
#[test]
fn keystore2_aes_key_op_fails_multi_padding_modes() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_invalid_1";

    let gen_params = authorizations::AuthSetBuilder::new()
        .no_auth_required()
        .algorithm(Algorithm::AES)
        .purpose(KeyPurpose::ENCRYPT)
        .purpose(KeyPurpose::DECRYPT)
        .key_size(128)
        .block_mode(BlockMode::ECB)
        .padding_mode(PaddingMode::PKCS7)
        .padding_mode(PaddingMode::NONE);

    let key_metadata = sec_level
        .generateKey(
            &KeyDescriptor {
                domain: Domain::APP,
                nspace: -1,
                alias: Some(alias.to_string()),
                blob: None,
            },
            None,
            &gen_params,
            0,
            b"entropy",
        )
        .unwrap();

    let op_params = authorizations::AuthSetBuilder::new()
        .purpose(KeyPurpose::ENCRYPT)
        .block_mode(BlockMode::ECB)
        .padding_mode(PaddingMode::PKCS7)
        .padding_mode(PaddingMode::NONE);

    let result = key_generations::map_ks_error(sec_level.createOperation(
        &key_metadata.key,
        &op_params,
        false,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_PADDING_MODE), result.unwrap_err());
}

/// Generate a AES-ECB key with unpadded mode. Try to create an operation using generated key
/// with PKCS7 padding mode. Test should fail to create an Operation with
/// `INCOMPATIBLE_PADDING_MODE` error code.
#[test]
fn keystore2_aes_key_op_fails_incompatible_padding() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_invalid_1";

    let key_metadata = key_generations::generate_aes_key(
        &sec_level,
        128,
        alias,
        &PaddingMode::NONE,
        &BlockMode::ECB,
        None,
    )
    .unwrap();

    let result = key_generations::map_ks_error(perform_sample_sym_key_encrypt_op(
        &sec_level,
        PaddingMode::PKCS7,
        BlockMode::ECB,
        &mut None,
        None,
        &key_metadata.key,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_PADDING_MODE), result.unwrap_err());
}

/// Generate a AES-ECB key with unpadded mode. Try to create an operation using generated key
/// with CBC block mode. Test should fail to create an Operation with
/// `INCOMPATIBLE_BLOCK_MODE` error code.
#[test]
fn keystore2_aes_key_op_fails_incompatible_blockmode() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_invalid_1";

    let key_metadata = key_generations::generate_aes_key(
        &sec_level,
        128,
        alias,
        &PaddingMode::NONE,
        &BlockMode::ECB,
        None,
    )
    .unwrap();

    let result = key_generations::map_ks_error(perform_sample_sym_key_encrypt_op(
        &sec_level,
        PaddingMode::NONE,
        BlockMode::CBC,
        &mut None,
        None,
        &key_metadata.key,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INCOMPATIBLE_BLOCK_MODE), result.unwrap_err());
}

/// Generate a AES-GCM key with `MIN_MAC_LENGTH`. Try to create an operation using this
/// generated key without providing `MAC_LENGTH`. Test should fail to create an operation with
/// `MISSING_MAC_LENGTH` error code.
#[test]
fn keystore2_aes_gcm_op_fails_missing_mac_len() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let mac_len = None;
    let min_mac_len = Some(128);

    let result = key_generations::map_ks_error(create_aes_key_and_operation(
        &sec_level,
        128,
        PaddingMode::NONE,
        BlockMode::GCM,
        mac_len,
        min_mac_len,
        &mut None,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::MISSING_MAC_LENGTH), result.unwrap_err());
}

/// Generate a AES-GCM key with `MIN_MAC_LENGTH`. Try to create an operation using this
/// generated key and  provide `MAC_LENGTH` < key's `MIN_MAC_LENGTH`. Test should fail to create
/// an operation with `INVALID_MAC_LENGTH` error code.
#[test]
fn keystore2_aes_gcm_op_fails_invalid_mac_len() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let mac_len = Some(96);
    let min_mac_len = Some(104);

    let result = key_generations::map_ks_error(create_aes_key_and_operation(
        &sec_level,
        128,
        PaddingMode::NONE,
        BlockMode::GCM,
        mac_len,
        min_mac_len,
        &mut None,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::INVALID_MAC_LENGTH), result.unwrap_err());
}

/// Generate a AES-GCM key with `MIN_MAC_LENGTH`. Try to create an operation using this
/// generated key and  provide `MAC_LENGTH` > 128. Test should fail to create an operation with
/// `UNSUPPORTED_MAC_LENGTH` error code.
#[test]
fn keystore2_aes_gcm_op_fails_unsupported_mac_len() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

    let result = key_generations::map_ks_error(create_aes_key_and_operation(
        &sec_level,
        128,
        PaddingMode::NONE,
        BlockMode::GCM,
        Some(256),
        Some(128),
        &mut None,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::UNSUPPORTED_MAC_LENGTH), result.unwrap_err());
}

/// Generate a AES-CBC-PKCS7 key without `CALLER_NONCE` authorization. Try to set nonce while
/// creating an operation using this generated key. Test should fail to create an operation with
/// `CALLER_NONCE_PROHIBITED` error code.
#[test]
fn keystore2_aes_key_op_fails_nonce_prohibited() {
    let keystore2 = get_keystore_service();
    let sec_level = keystore2.getSecurityLevel(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();
    let alias = "aes_key_test_nonce_1";
    let mut nonce = Some(vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    let key_metadata = key_generations::generate_aes_key(
        &sec_level,
        128,
        alias,
        &PaddingMode::PKCS7,
        &BlockMode::CBC,
        None,
    )
    .unwrap();

    let result = key_generations::map_ks_error(perform_sample_sym_key_encrypt_op(
        &sec_level,
        PaddingMode::NONE,
        BlockMode::CBC,
        &mut nonce,
        None,
        &key_metadata.key,
    ));
    assert!(result.is_err());
    assert_eq!(Error::Km(ErrorCode::CALLER_NONCE_PROHIBITED), result.unwrap_err());
}
