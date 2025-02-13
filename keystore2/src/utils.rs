// Copyright 2020, The Android Open Source Project
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

//! This module implements utility functions used by the Keystore 2.0 service
//! implementation.

use crate::error::{map_binder_status, map_km_error, Error, ErrorCode};
use crate::key_parameter::KeyParameter;
use crate::ks_err;
use crate::permission;
use crate::permission::{KeyPerm, KeyPermSet, KeystorePerm};
use crate::{
    database::{KeyType, KeystoreDB},
    globals::LEGACY_IMPORTER,
    km_compat,
    raw_device::KeyMintDevice,
};
use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
    IKeyMintDevice::IKeyMintDevice, KeyCharacteristics::KeyCharacteristics,
    KeyParameter::KeyParameter as KmKeyParameter, Tag::Tag,
};
use android_os_permissions_aidl::aidl::android::os::IPermissionController;
use android_security_apc::aidl::android::security::apc::{
    IProtectedConfirmation::{FLAG_UI_OPTION_INVERTED, FLAG_UI_OPTION_MAGNIFIED},
    ResponseCode::ResponseCode as ApcResponseCode,
};
use android_system_keystore2::aidl::android::system::keystore2::{
    Authorization::Authorization, Domain::Domain, KeyDescriptor::KeyDescriptor,
};
use anyhow::{Context, Result};
use binder::{Strong, ThreadState};
use keystore2_apc_compat::{
    ApcCompatUiOptions, APC_COMPAT_ERROR_ABORTED, APC_COMPAT_ERROR_CANCELLED,
    APC_COMPAT_ERROR_IGNORED, APC_COMPAT_ERROR_OK, APC_COMPAT_ERROR_OPERATION_PENDING,
    APC_COMPAT_ERROR_SYSTEM_ERROR,
};
use keystore2_crypto::{aes_gcm_decrypt, aes_gcm_encrypt, ZVec};
use std::iter::IntoIterator;

/// This function uses its namesake in the permission module and in
/// combination with with_calling_sid from the binder crate to check
/// if the caller has the given keystore permission.
pub fn check_keystore_permission(perm: KeystorePerm) -> anyhow::Result<()> {
    ThreadState::with_calling_sid(|calling_sid| {
        permission::check_keystore_permission(
            calling_sid
                .ok_or_else(Error::sys)
                .context(ks_err!("Cannot check permission without calling_sid."))?,
            perm,
        )
    })
}

/// This function uses its namesake in the permission module and in
/// combination with with_calling_sid from the binder crate to check
/// if the caller has the given grant permission.
pub fn check_grant_permission(access_vec: KeyPermSet, key: &KeyDescriptor) -> anyhow::Result<()> {
    ThreadState::with_calling_sid(|calling_sid| {
        permission::check_grant_permission(
            calling_sid
                .ok_or_else(Error::sys)
                .context(ks_err!("Cannot check permission without calling_sid."))?,
            access_vec,
            key,
        )
    })
}

/// This function uses its namesake in the permission module and in
/// combination with with_calling_sid from the binder crate to check
/// if the caller has the given key permission.
pub fn check_key_permission(
    perm: KeyPerm,
    key: &KeyDescriptor,
    access_vector: &Option<KeyPermSet>,
) -> anyhow::Result<()> {
    ThreadState::with_calling_sid(|calling_sid| {
        permission::check_key_permission(
            ThreadState::get_calling_uid(),
            calling_sid
                .ok_or_else(Error::sys)
                .context(ks_err!("Cannot check permission without calling_sid."))?,
            perm,
            key,
            access_vector,
        )
    })
}

/// This function checks whether a given tag corresponds to the access of device identifiers.
pub fn is_device_id_attestation_tag(tag: Tag) -> bool {
    matches!(
        tag,
        Tag::ATTESTATION_ID_IMEI
            | Tag::ATTESTATION_ID_MEID
            | Tag::ATTESTATION_ID_SERIAL
            | Tag::DEVICE_UNIQUE_ATTESTATION
            | Tag::ATTESTATION_ID_SECOND_IMEI
    )
}

/// This function checks whether the calling app has the Android permissions needed to attest device
/// identifiers. It throws an error if the permissions cannot be verified or if the caller doesn't
/// have the right permissions. Otherwise it returns silently.
pub fn check_device_attestation_permissions() -> anyhow::Result<()> {
    check_android_permission("android.permission.READ_PRIVILEGED_PHONE_STATE")
}

/// This function checks whether the calling app has the Android permissions needed to attest the
/// device-unique identifier. It throws an error if the permissions cannot be verified or if the
/// caller doesn't have the right permissions. Otherwise it returns silently.
pub fn check_unique_id_attestation_permissions() -> anyhow::Result<()> {
    check_android_permission("android.permission.REQUEST_UNIQUE_ID_ATTESTATION")
}

fn check_android_permission(permission: &str) -> anyhow::Result<()> {
    let permission_controller: Strong<dyn IPermissionController::IPermissionController> =
        binder::get_interface("permission")?;

    let binder_result = {
        let _wp = watchdog::watch_millis(
            "In check_device_attestation_permissions: calling checkPermission.",
            500,
        );
        permission_controller.checkPermission(
            permission,
            ThreadState::get_calling_pid(),
            ThreadState::get_calling_uid() as i32,
        )
    };
    let has_permissions =
        map_binder_status(binder_result).context(ks_err!("checkPermission failed"))?;
    match has_permissions {
        true => Ok(()),
        false => Err(Error::Km(ErrorCode::CANNOT_ATTEST_IDS))
            .context(ks_err!("caller does not have the permission to attest device IDs")),
    }
}

/// Converts a set of key characteristics as returned from KeyMint into the internal
/// representation of the keystore service.
pub fn key_characteristics_to_internal(
    key_characteristics: Vec<KeyCharacteristics>,
) -> Vec<KeyParameter> {
    key_characteristics
        .into_iter()
        .flat_map(|aidl_key_char| {
            let sec_level = aidl_key_char.securityLevel;
            aidl_key_char
                .authorizations
                .into_iter()
                .map(move |aidl_kp| KeyParameter::new(aidl_kp.into(), sec_level))
        })
        .collect()
}

/// Upgrade a keyblob then invoke both the `new_blob_handler` and the `km_op` closures.  On success
/// a tuple of the `km_op`s result and the optional upgraded blob is returned.
fn upgrade_keyblob_and_perform_op<T, KmOp, NewBlobHandler>(
    km_dev: &dyn IKeyMintDevice,
    key_blob: &[u8],
    upgrade_params: &[KmKeyParameter],
    km_op: KmOp,
    new_blob_handler: NewBlobHandler,
) -> Result<(T, Option<Vec<u8>>)>
where
    KmOp: Fn(&[u8]) -> Result<T, Error>,
    NewBlobHandler: FnOnce(&[u8]) -> Result<()>,
{
    let upgraded_blob = {
        let _wp = watchdog::watch_millis(
            "In utils::upgrade_keyblob_and_perform_op: calling upgradeKey.",
            500,
        );
        map_km_error(km_dev.upgradeKey(key_blob, upgrade_params))
    }
    .context(ks_err!("Upgrade failed."))?;

    new_blob_handler(&upgraded_blob).context(ks_err!("calling new_blob_handler."))?;

    km_op(&upgraded_blob)
        .map(|v| (v, Some(upgraded_blob)))
        .context(ks_err!("Calling km_op after upgrade."))
}

/// This function can be used to upgrade key blobs on demand. The return value of
/// `km_op` is inspected and if ErrorCode::KEY_REQUIRES_UPGRADE is encountered,
/// an attempt is made to upgrade the key blob. On success `new_blob_handler` is called
/// with the upgraded blob as argument. Then `km_op` is called a second time with the
/// upgraded blob as argument. On success a tuple of the `km_op`s result and the
/// optional upgraded blob is returned.
pub fn upgrade_keyblob_if_required_with<T, KmOp, NewBlobHandler>(
    km_dev: &dyn IKeyMintDevice,
    km_dev_version: i32,
    key_blob: &[u8],
    upgrade_params: &[KmKeyParameter],
    km_op: KmOp,
    new_blob_handler: NewBlobHandler,
) -> Result<(T, Option<Vec<u8>>)>
where
    KmOp: Fn(&[u8]) -> Result<T, Error>,
    NewBlobHandler: FnOnce(&[u8]) -> Result<()>,
{
    match km_op(key_blob) {
        Err(Error::Km(ErrorCode::KEY_REQUIRES_UPGRADE)) => upgrade_keyblob_and_perform_op(
            km_dev,
            key_blob,
            upgrade_params,
            km_op,
            new_blob_handler,
        ),
        // Some devices have been known to upgrade their Keymaster device to be a KeyMint
        // device with a new release of Android.  If this is the case, then any pre-upgrade
        // keyblobs will have the km_compat prefix attached to them.
        //
        // This prefix gets stripped by the km_compat layer when used pre-upgrade, but after
        // the upgrade the keyblob will be passed as-is to the KeyMint device, which probably
        // won't expect to see the km_compat prefix.
        //
        // So if a keyblob:
        //   a) gets rejected with INVALID_KEY_BLOB
        //   b) when sent to a KeyMint (not km_compat) device
        //   c) and has the km_compat magic prefix
        //   d) and was not a software-emulated key pre-upgrade
        // then strip the prefix and attempt a key upgrade.
        Err(Error::Km(ErrorCode::INVALID_KEY_BLOB))
            if km_dev_version >= KeyMintDevice::KEY_MINT_V1
                && key_blob.starts_with(km_compat::KEYMASTER_BLOB_HW_PREFIX) =>
        {
            log::info!("found apparent km_compat(Keymaster) blob, attempt strip-and-upgrade");
            let inner_keyblob = &key_blob[km_compat::KEYMASTER_BLOB_HW_PREFIX.len()..];
            upgrade_keyblob_and_perform_op(
                km_dev,
                inner_keyblob,
                upgrade_params,
                km_op,
                new_blob_handler,
            )
        }
        r => r.map(|v| (v, None)).context(ks_err!("Calling km_op.")),
    }
}

/// Converts a set of key characteristics from the internal representation into a set of
/// Authorizations as they are used to convey key characteristics to the clients of keystore.
pub fn key_parameters_to_authorizations(
    parameters: Vec<crate::key_parameter::KeyParameter>,
) -> Vec<Authorization> {
    parameters.into_iter().map(|p| p.into_authorization()).collect()
}

#[allow(clippy::unnecessary_cast)]
/// This returns the current time (in milliseconds) as an instance of a monotonic clock,
/// by invoking the system call since Rust does not support getting monotonic time instance
/// as an integer.
pub fn get_current_time_in_milliseconds() -> i64 {
    let mut current_time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: The pointer is valid because it comes from a reference, and clock_gettime doesn't
    // retain it beyond the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut current_time) };
    current_time.tv_sec as i64 * 1000 + (current_time.tv_nsec as i64 / 1_000_000)
}

/// Converts a response code as returned by the Android Protected Confirmation HIDL compatibility
/// module (keystore2_apc_compat) into a ResponseCode as defined by the APC AIDL
/// (android.security.apc) spec.
pub fn compat_2_response_code(rc: u32) -> ApcResponseCode {
    match rc {
        APC_COMPAT_ERROR_OK => ApcResponseCode::OK,
        APC_COMPAT_ERROR_CANCELLED => ApcResponseCode::CANCELLED,
        APC_COMPAT_ERROR_ABORTED => ApcResponseCode::ABORTED,
        APC_COMPAT_ERROR_OPERATION_PENDING => ApcResponseCode::OPERATION_PENDING,
        APC_COMPAT_ERROR_IGNORED => ApcResponseCode::IGNORED,
        APC_COMPAT_ERROR_SYSTEM_ERROR => ApcResponseCode::SYSTEM_ERROR,
        _ => ApcResponseCode::SYSTEM_ERROR,
    }
}

/// Converts the UI Options flags as defined by the APC AIDL (android.security.apc) spec into
/// UI Options flags as defined by the Android Protected Confirmation HIDL compatibility
/// module (keystore2_apc_compat).
pub fn ui_opts_2_compat(opt: i32) -> ApcCompatUiOptions {
    ApcCompatUiOptions {
        inverted: (opt & FLAG_UI_OPTION_INVERTED) != 0,
        magnified: (opt & FLAG_UI_OPTION_MAGNIFIED) != 0,
    }
}

/// AID offset for uid space partitioning.
pub const AID_USER_OFFSET: u32 = rustutils::users::AID_USER_OFFSET;

/// AID of the keystore process itself, used for keys that
/// keystore generates for its own use.
pub const AID_KEYSTORE: u32 = rustutils::users::AID_KEYSTORE;

/// Extracts the android user from the given uid.
pub fn uid_to_android_user(uid: u32) -> u32 {
    rustutils::users::multiuser_get_user_id(uid)
}

/// Merges and filters two lists of key descriptors. The first input list, legacy_descriptors,
/// is assumed to not be sorted or filtered. As such, all key descriptors in that list whose
/// alias is less than, or equal to, start_past_alias (if provided) will be removed.
/// This list will then be merged with the second list, db_descriptors. The db_descriptors list
/// is assumed to be sorted and filtered so the output list will be sorted prior to returning.
/// The returned value is a list of KeyDescriptor objects whose alias is greater than
/// start_past_alias, sorted and de-duplicated.
fn merge_and_filter_key_entry_lists(
    legacy_descriptors: &[KeyDescriptor],
    db_descriptors: &[KeyDescriptor],
    start_past_alias: Option<&str>,
) -> Vec<KeyDescriptor> {
    let mut result: Vec<KeyDescriptor> =
        match start_past_alias {
            Some(past_alias) => legacy_descriptors
                .iter()
                .filter(|kd| {
                    if let Some(alias) = &kd.alias {
                        alias.as_str() > past_alias
                    } else {
                        false
                    }
                })
                .cloned()
                .collect(),
            None => legacy_descriptors.to_vec(),
        };

    result.extend_from_slice(db_descriptors);
    result.sort_unstable();
    result.dedup();
    result
}

fn estimate_safe_amount_to_return(
    key_descriptors: &[KeyDescriptor],
    response_size_limit: usize,
) -> usize {
    let mut items_to_return = 0;
    let mut returned_bytes: usize = 0;
    // Estimate the transaction size to avoid returning more items than what
    // could fit in a binder transaction.
    for kd in key_descriptors.iter() {
        // 4 bytes for the Domain enum
        // 8 bytes for the Namespace long.
        returned_bytes += 4 + 8;
        // Size of the alias string. Includes 4 bytes for length encoding.
        if let Some(alias) = &kd.alias {
            returned_bytes += 4 + alias.len();
        }
        // Size of the blob. Includes 4 bytes for length encoding.
        if let Some(blob) = &kd.blob {
            returned_bytes += 4 + blob.len();
        }
        // The binder transaction size limit is 1M. Empirical measurements show
        // that the binder overhead is 60% (to be confirmed). So break after
        // 350KB and return a partial list.
        if returned_bytes > response_size_limit {
            log::warn!(
                "Key descriptors list ({} items) may exceed binder \
                       size, returning {} items est {} bytes.",
                key_descriptors.len(),
                items_to_return,
                returned_bytes
            );
            break;
        }
        items_to_return += 1;
    }
    items_to_return
}

/// List all key aliases for a given domain + namespace. whose alias is greater
/// than start_past_alias (if provided).
pub fn list_key_entries(
    db: &mut KeystoreDB,
    domain: Domain,
    namespace: i64,
    start_past_alias: Option<&str>,
) -> Result<Vec<KeyDescriptor>> {
    let legacy_key_descriptors: Vec<KeyDescriptor> = LEGACY_IMPORTER
        .list_uid(domain, namespace)
        .context(ks_err!("Trying to list legacy keys."))?;

    // The results from the database will be sorted and unique
    let db_key_descriptors: Vec<KeyDescriptor> = db
        .list_past_alias(domain, namespace, KeyType::Client, start_past_alias)
        .context(ks_err!("Trying to list keystore database past alias."))?;

    let merged_key_entries = merge_and_filter_key_entry_lists(
        &legacy_key_descriptors,
        &db_key_descriptors,
        start_past_alias,
    );

    const RESPONSE_SIZE_LIMIT: usize = 358400;
    let safe_amount_to_return =
        estimate_safe_amount_to_return(&merged_key_entries, RESPONSE_SIZE_LIMIT);
    Ok(merged_key_entries[..safe_amount_to_return].to_vec())
}

/// Count all key aliases for a given domain + namespace.
pub fn count_key_entries(db: &mut KeystoreDB, domain: Domain, namespace: i64) -> Result<i32> {
    let legacy_keys = LEGACY_IMPORTER
        .list_uid(domain, namespace)
        .context(ks_err!("Trying to list legacy keys."))?;

    let num_keys_in_db = db.count_keys(domain, namespace, KeyType::Client)?;

    Ok((legacy_keys.len() + num_keys_in_db) as i32)
}

/// This module provides helpers for simplified use of the watchdog module.
#[cfg(feature = "watchdog")]
pub mod watchdog {
    pub use crate::watchdog::WatchPoint;
    use crate::watchdog::Watchdog;
    use lazy_static::lazy_static;
    use std::sync::Arc;
    use std::time::Duration;

    lazy_static! {
        /// A Watchdog thread, that can be used to create watch points.
        static ref WD: Arc<Watchdog> = Watchdog::new(Duration::from_secs(10));
    }

    /// Sets a watch point with `id` and a timeout of `millis` milliseconds.
    pub fn watch_millis(id: &'static str, millis: u64) -> Option<WatchPoint> {
        Watchdog::watch(&WD, id, Duration::from_millis(millis))
    }

    /// Like `watch_millis` but with a callback that is called every time a report
    /// is printed about this watch point.
    pub fn watch_millis_with(
        id: &'static str,
        millis: u64,
        callback: impl Fn() -> String + Send + 'static,
    ) -> Option<WatchPoint> {
        Watchdog::watch_with(&WD, id, Duration::from_millis(millis), callback)
    }
}

/// Trait implemented by objects that can be used to decrypt cipher text using AES-GCM.
pub trait AesGcm {
    /// Deciphers `data` using the initialization vector `iv` and AEAD tag `tag`
    /// and AES-GCM. The implementation provides the key material and selects
    /// the implementation variant, e.g., AES128 or AES265.
    fn decrypt(&self, data: &[u8], iv: &[u8], tag: &[u8]) -> Result<ZVec>;

    /// Encrypts `data` and returns the ciphertext, the initialization vector `iv`
    /// and AEAD tag `tag`. The implementation provides the key material and selects
    /// the implementation variant, e.g., AES128 or AES265.
    fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)>;
}

/// Marks an object as AES-GCM key.
pub trait AesGcmKey {
    /// Provides access to the raw key material.
    fn key(&self) -> &[u8];
}

impl<T: AesGcmKey> AesGcm for T {
    fn decrypt(&self, data: &[u8], iv: &[u8], tag: &[u8]) -> Result<ZVec> {
        aes_gcm_decrypt(data, iv, tag, self.key()).context(ks_err!("Decryption failed"))
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        aes_gcm_encrypt(plaintext, self.key()).context(ks_err!("Encryption failed."))
    }
}

/// This module provides empty/noop implementations of the watch dog utility functions.
#[cfg(not(feature = "watchdog"))]
pub mod watchdog {
    /// Noop watch point.
    pub struct WatchPoint();
    /// Sets a Noop watch point.
    fn watch_millis(_: &'static str, _: u64) -> Option<WatchPoint> {
        None
    }

    pub fn watch_millis_with(
        _: &'static str,
        _: u64,
        _: impl Fn() -> String + Send + 'static,
    ) -> Option<WatchPoint> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn check_device_attestation_permissions_test() -> Result<()> {
        check_device_attestation_permissions().or_else(|error| {
            match error.root_cause().downcast_ref::<Error>() {
                // Expected: the context for this test might not be allowed to attest device IDs.
                Some(Error::Km(ErrorCode::CANNOT_ATTEST_IDS)) => Ok(()),
                // Other errors are unexpected
                _ => Err(error),
            }
        })
    }

    fn create_key_descriptors_from_aliases(key_aliases: &[&str]) -> Vec<KeyDescriptor> {
        key_aliases
            .iter()
            .map(|key_alias| KeyDescriptor {
                domain: Domain::APP,
                nspace: 0,
                alias: Some(key_alias.to_string()),
                blob: None,
            })
            .collect::<Vec<KeyDescriptor>>()
    }

    fn aliases_from_key_descriptors(key_descriptors: &[KeyDescriptor]) -> Vec<String> {
        key_descriptors
            .iter()
            .map(
                |kd| {
                    if let Some(alias) = &kd.alias {
                        String::from(alias)
                    } else {
                        String::from("")
                    }
                },
            )
            .collect::<Vec<String>>()
    }

    #[test]
    fn test_safe_amount_to_return() -> Result<()> {
        let key_aliases = vec!["key1", "key2", "key3"];
        let key_descriptors = create_key_descriptors_from_aliases(&key_aliases);

        assert_eq!(estimate_safe_amount_to_return(&key_descriptors, 20), 1);
        assert_eq!(estimate_safe_amount_to_return(&key_descriptors, 50), 2);
        assert_eq!(estimate_safe_amount_to_return(&key_descriptors, 100), 3);
        Ok(())
    }

    #[test]
    fn test_merge_and_sort_lists_without_filtering() -> Result<()> {
        let legacy_key_aliases = vec!["key_c", "key_a", "key_b"];
        let legacy_key_descriptors = create_key_descriptors_from_aliases(&legacy_key_aliases);
        let db_key_aliases = vec!["key_a", "key_d"];
        let db_key_descriptors = create_key_descriptors_from_aliases(&db_key_aliases);
        let result =
            merge_and_filter_key_entry_lists(&legacy_key_descriptors, &db_key_descriptors, None);
        assert_eq!(aliases_from_key_descriptors(&result), vec!["key_a", "key_b", "key_c", "key_d"]);
        Ok(())
    }

    #[test]
    fn test_merge_and_sort_lists_with_filtering() -> Result<()> {
        let legacy_key_aliases = vec!["key_f", "key_a", "key_e", "key_b"];
        let legacy_key_descriptors = create_key_descriptors_from_aliases(&legacy_key_aliases);
        let db_key_aliases = vec!["key_c", "key_g"];
        let db_key_descriptors = create_key_descriptors_from_aliases(&db_key_aliases);
        let result = merge_and_filter_key_entry_lists(
            &legacy_key_descriptors,
            &db_key_descriptors,
            Some("key_b"),
        );
        assert_eq!(aliases_from_key_descriptors(&result), vec!["key_c", "key_e", "key_f", "key_g"]);
        Ok(())
    }

    #[test]
    fn test_merge_and_sort_lists_with_filtering_and_dups() -> Result<()> {
        let legacy_key_aliases = vec!["key_f", "key_a", "key_e", "key_b"];
        let legacy_key_descriptors = create_key_descriptors_from_aliases(&legacy_key_aliases);
        let db_key_aliases = vec!["key_d", "key_e", "key_g"];
        let db_key_descriptors = create_key_descriptors_from_aliases(&db_key_aliases);
        let result = merge_and_filter_key_entry_lists(
            &legacy_key_descriptors,
            &db_key_descriptors,
            Some("key_c"),
        );
        assert_eq!(aliases_from_key_descriptors(&result), vec!["key_d", "key_e", "key_f", "key_g"]);
        Ok(())
    }
}
