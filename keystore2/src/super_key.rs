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

#![allow(dead_code)]

use crate::{
    database::BlobMetaData, database::BlobMetaEntry, database::EncryptedBy, database::KeyEntry,
    database::KeyType, database::KeystoreDB, enforcements::Enforcements, error::Error,
    error::ResponseCode, key_parameter::KeyParameter, legacy_blob::LegacyBlobLoader,
    legacy_migrator::LegacyMigrator,
};
use android_system_keystore2::aidl::android::system::keystore2::Domain::Domain;
use anyhow::{Context, Result};
use keystore2_crypto::{
    aes_gcm_decrypt, aes_gcm_encrypt, derive_key_from_password, generate_aes256_key, generate_salt,
    ZVec, AES_256_KEY_LENGTH,
};
use std::ops::Deref;
use std::{
    collections::HashMap,
    sync::Arc,
    sync::{Mutex, Weak},
};

type UserId = u32;

#[derive(Default)]
struct UserSuperKeys {
    /// The per boot key is used for LSKF binding of authentication bound keys. There is one
    /// key per android user. The key is stored on flash encrypted with a key derived from a
    /// secret, that is itself derived from the user's lock screen knowledge factor (LSKF).
    /// When the user unlocks the device for the first time, this key is unlocked, i.e., decrypted,
    /// and stays memory resident until the device reboots.
    per_boot: Option<SuperKey>,
    /// The screen lock key works like the per boot key with the distinction that it is cleared
    /// from memory when the screen lock is engaged.
    /// TODO the life cycle is not fully implemented at this time.
    screen_lock: Option<Arc<ZVec>>,
}

#[derive(Default, Clone)]
pub struct SuperKey {
    key: Arc<ZVec>,
    // id of the super key in the database.
    id: i64,
}

impl SuperKey {
    pub fn get_key(&self) -> &Arc<ZVec> {
        &self.key
    }

    pub fn get_id(&self) -> i64 {
        self.id
    }
}

#[derive(Default)]
struct SkmState {
    user_keys: HashMap<UserId, UserSuperKeys>,
    key_index: HashMap<i64, Weak<ZVec>>,
}

#[derive(Default)]
pub struct SuperKeyManager {
    data: Mutex<SkmState>,
}

impl SuperKeyManager {
    pub fn new() -> Self {
        Self { data: Mutex::new(Default::default()) }
    }

    pub fn forget_screen_lock_key_for_user(&self, user: UserId) {
        let mut data = self.data.lock().unwrap();
        if let Some(usk) = data.user_keys.get_mut(&user) {
            usk.screen_lock = None;
        }
    }

    pub fn forget_screen_lock_keys(&self) {
        let mut data = self.data.lock().unwrap();
        for (_, usk) in data.user_keys.iter_mut() {
            usk.screen_lock = None;
        }
    }

    pub fn forget_all_keys_for_user(&self, user: UserId) {
        let mut data = self.data.lock().unwrap();
        data.user_keys.remove(&user);
    }

    pub fn forget_all_keys(&self) {
        let mut data = self.data.lock().unwrap();
        data.user_keys.clear();
        data.key_index.clear();
    }

    fn install_per_boot_key_for_user(&self, user: UserId, super_key: SuperKey) {
        let mut data = self.data.lock().unwrap();
        data.key_index.insert(super_key.id, Arc::downgrade(&(super_key.key)));
        data.user_keys.entry(user).or_default().per_boot = Some(super_key);
    }

    fn get_key(&self, key_id: &i64) -> Option<Arc<ZVec>> {
        self.data.lock().unwrap().key_index.get(key_id).and_then(|k| k.upgrade())
    }

    pub fn get_per_boot_key_by_user_id(&self, user_id: u32) -> Option<SuperKey> {
        let data = self.data.lock().unwrap();
        data.user_keys.get(&user_id).map(|e| e.per_boot.clone()).flatten()
    }

    /// This function unlocks the super keys for a given user.
    /// This means the key is loaded from the database, decrypted and placed in the
    /// super key cache. If there is no such key a new key is created, encrypted with
    /// a key derived from the given password and stored in the database.
    pub fn unlock_user_key(
        &self,
        db: &mut KeystoreDB,
        user: UserId,
        pw: &[u8],
        legacy_blob_loader: &LegacyBlobLoader,
    ) -> Result<()> {
        let (_, entry) = db
            .get_or_create_key_with(
                Domain::APP,
                user as u64 as i64,
                KeystoreDB::USER_SUPER_KEY_ALIAS,
                crate::database::KEYSTORE_UUID,
                || {
                    // For backward compatibility we need to check if there is a super key present.
                    let super_key = legacy_blob_loader
                        .load_super_key(user, pw)
                        .context("In create_new_key: Failed to load legacy key blob.")?;
                    let super_key = match super_key {
                        None => {
                            // No legacy file was found. So we generate a new key.
                            generate_aes256_key()
                                .context("In create_new_key: Failed to generate AES 256 key.")?
                        }
                        Some(key) => key,
                    };
                    // Regardless of whether we loaded an old AES128 key or generated a new AES256
                    // key as the super key, we derive a AES256 key from the password and re-encrypt
                    // the super key before we insert it in the database. The length of the key is
                    // preserved by the encryption so we don't need any extra flags to inform us
                    // which algorithm to use it with.
                    Self::encrypt_with_password(&super_key, pw).context("In create_new_key.")
                },
            )
            .context("In unlock_user_key: Failed to get key id.")?;

        self.populate_cache_from_super_key_blob(user, entry, pw).context("In unlock_user_key.")?;
        Ok(())
    }

    /// Unwraps an encrypted key blob given metadata identifying the encryption key.
    /// The function queries `metadata.encrypted_by()` to determine the encryption key.
    /// It then check if the required key is memory resident, and if so decrypts the
    /// blob.
    pub fn unwrap_key<'a>(&self, blob: &'a [u8], metadata: &BlobMetaData) -> Result<KeyBlob<'a>> {
        match metadata.encrypted_by() {
            Some(EncryptedBy::KeyId(key_id)) => match self.get_key(key_id) {
                Some(key) => Ok(KeyBlob::Sensitive(
                    Self::unwrap_key_with_key(blob, metadata, &key).context("In unwrap_key.")?,
                    SuperKey { key: key.clone(), id: *key_id },
                )),
                None => Err(Error::Rc(ResponseCode::LOCKED))
                    .context("In unwrap_key: Key is not usable until the user entered their LSKF."),
            },
            _ => Err(Error::Rc(ResponseCode::VALUE_CORRUPTED))
                .context("In unwrap_key: Cannot determined wrapping key."),
        }
    }

    /// Unwraps an encrypted key blob given an encryption key.
    fn unwrap_key_with_key(blob: &[u8], metadata: &BlobMetaData, key: &[u8]) -> Result<ZVec> {
        match (metadata.iv(), metadata.aead_tag()) {
            (Some(iv), Some(tag)) => aes_gcm_decrypt(blob, iv, tag, key)
                .context("In unwrap_key_with_key: Failed to decrypt the key blob."),
            (iv, tag) => Err(Error::Rc(ResponseCode::VALUE_CORRUPTED)).context(format!(
                concat!(
                    "In unwrap_key_with_key: Key has incomplete metadata.",
                    "Present: iv: {}, aead_tag: {}."
                ),
                iv.is_some(),
                tag.is_some(),
            )),
        }
    }

    /// Checks if user has setup LSKF, even when super key cache is empty for the user.
    pub fn super_key_exists_in_db_for_user(
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        user_id: u32,
    ) -> Result<bool> {
        let key_in_db = db
            .key_exists(
                Domain::APP,
                user_id as u64 as i64,
                KeystoreDB::USER_SUPER_KEY_ALIAS,
                KeyType::Super,
            )
            .context("In super_key_exists_in_db_for_user.")?;

        if key_in_db {
            Ok(key_in_db)
        } else {
            legacy_migrator
                .has_super_key(user_id)
                .context("In super_key_exists_in_db_for_user: Trying to query legacy db.")
        }
    }

    /// Checks if user has already setup LSKF (i.e. a super key is persisted in the database or the
    /// legacy database). If not, return Uninitialized state.
    /// Otherwise, decrypt the super key from the password and return LskfUnlocked state.
    pub fn check_and_unlock_super_key(
        &self,
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        user_id: u32,
        pw: &[u8],
    ) -> Result<UserState> {
        let result = legacy_migrator
            .with_try_migrate_super_key(user_id, pw, || db.load_super_key(user_id))
            .context("In check_and_unlock_super_key. Failed to load super key")?;

        match result {
            Some((_, entry)) => {
                let super_key = self
                    .populate_cache_from_super_key_blob(user_id, entry, pw)
                    .context("In check_and_unlock_super_key.")?;
                Ok(UserState::LskfUnlocked(super_key))
            }
            None => Ok(UserState::Uninitialized),
        }
    }

    /// Checks if user has already setup LSKF (i.e. a super key is persisted in the database or the
    /// legacy database). If so, return LskfLocked state.
    /// If the password is provided, generate a new super key, encrypt with the password,
    /// store in the database and populate the super key cache for the new user
    /// and return LskfUnlocked state.
    /// If the password is not provided, return Uninitialized state.
    pub fn check_and_initialize_super_key(
        &self,
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        user_id: u32,
        pw: Option<&[u8]>,
    ) -> Result<UserState> {
        let super_key_exists_in_db =
            Self::super_key_exists_in_db_for_user(db, legacy_migrator, user_id).context(
                "In check_and_initialize_super_key. Failed to check if super key exists.",
            )?;
        if super_key_exists_in_db {
            Ok(UserState::LskfLocked)
        } else if let Some(pw) = pw {
            //generate a new super key.
            let super_key = generate_aes256_key()
                .context("In check_and_initialize_super_key: Failed to generate AES 256 key.")?;
            //derive an AES256 key from the password and re-encrypt the super key
            //before we insert it in the database.
            let (encrypted_super_key, blob_metadata) = Self::encrypt_with_password(&super_key, pw)
                .context("In check_and_initialize_super_key.")?;

            let key_entry = db
                .store_super_key(user_id, &(&encrypted_super_key, &blob_metadata))
                .context("In check_and_initialize_super_key. Failed to store super key.")?;

            let super_key = self
                .populate_cache_from_super_key_blob(user_id, key_entry, pw)
                .context("In check_and_initialize_super_key.")?;
            Ok(UserState::LskfUnlocked(super_key))
        } else {
            Ok(UserState::Uninitialized)
        }
    }

    //helper function to populate super key cache from the super key blob loaded from the database
    fn populate_cache_from_super_key_blob(
        &self,
        user_id: u32,
        entry: KeyEntry,
        pw: &[u8],
    ) -> Result<SuperKey> {
        let super_key = Self::extract_super_key_from_key_entry(entry, pw).context(
            "In populate_cache_from_super_key_blob. Failed to extract super key from key entry",
        )?;
        self.install_per_boot_key_for_user(user_id, super_key.clone());
        Ok(super_key)
    }

    /// Extracts super key from the entry loaded from the database
    pub fn extract_super_key_from_key_entry(entry: KeyEntry, pw: &[u8]) -> Result<SuperKey> {
        if let Some((blob, metadata)) = entry.key_blob_info() {
            let key = match (
                metadata.encrypted_by(),
                metadata.salt(),
                metadata.iv(),
                metadata.aead_tag(),
            ) {
                (Some(&EncryptedBy::Password), Some(salt), Some(iv), Some(tag)) => {
                    let key = derive_key_from_password(pw, Some(salt), AES_256_KEY_LENGTH).context(
                    "In extract_super_key_from_key_entry: Failed to generate key from password.",
                )?;

                    aes_gcm_decrypt(blob, iv, tag, &key).context(
                        "In extract_super_key_from_key_entry: Failed to decrypt key blob.",
                    )?
                }
                (enc_by, salt, iv, tag) => {
                    return Err(Error::Rc(ResponseCode::VALUE_CORRUPTED)).context(format!(
                        concat!(
                        "In extract_super_key_from_key_entry: Super key has incomplete metadata.",
                        "Present: encrypted_by: {}, salt: {}, iv: {}, aead_tag: {}."
                    ),
                        enc_by.is_some(),
                        salt.is_some(),
                        iv.is_some(),
                        tag.is_some()
                    ));
                }
            };
            Ok(SuperKey { key: Arc::new(key), id: entry.id() })
        } else {
            Err(Error::Rc(ResponseCode::VALUE_CORRUPTED))
                .context("In extract_super_key_from_key_entry: No key blob info.")
        }
    }

    /// Encrypts the super key from a key derived from the password, before storing in the database.
    pub fn encrypt_with_password(super_key: &[u8], pw: &[u8]) -> Result<(Vec<u8>, BlobMetaData)> {
        let salt = generate_salt().context("In encrypt_with_password: Failed to generate salt.")?;
        let derived_key = derive_key_from_password(pw, Some(&salt), AES_256_KEY_LENGTH)
            .context("In encrypt_with_password: Failed to derive password.")?;
        let mut metadata = BlobMetaData::new();
        metadata.add(BlobMetaEntry::EncryptedBy(EncryptedBy::Password));
        metadata.add(BlobMetaEntry::Salt(salt));
        let (encrypted_key, iv, tag) = aes_gcm_encrypt(super_key, &derived_key)
            .context("In encrypt_with_password: Failed to encrypt new super key.")?;
        metadata.add(BlobMetaEntry::Iv(iv));
        metadata.add(BlobMetaEntry::AeadTag(tag));
        Ok((encrypted_key, metadata))
    }

    // Encrypt the given key blob with the user's super key, if the super key exists and the device
    // is unlocked. If the super key exists and the device is locked, or LSKF is not setup,
    // return error. Note that it is out of the scope of this function to check if super encryption
    // is required. Such check should be performed before calling this function.
    fn super_encrypt_on_key_init(
        &self,
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        user_id: u32,
        key_blob: &[u8],
    ) -> Result<(Vec<u8>, BlobMetaData)> {
        match UserState::get(db, legacy_migrator, self, user_id)
            .context("In super_encrypt. Failed to get user state.")?
        {
            UserState::LskfUnlocked(super_key) => {
                Self::encrypt_with_super_key(key_blob, &super_key)
                    .context("In super_encrypt_on_key_init. Failed to encrypt the key.")
            }
            UserState::LskfLocked => {
                Err(Error::Rc(ResponseCode::LOCKED)).context("In super_encrypt. Device is locked.")
            }
            UserState::Uninitialized => Err(Error::Rc(ResponseCode::UNINITIALIZED))
                .context("In super_encrypt. LSKF is not setup for the user."),
        }
    }

    //Helper function to encrypt a key with the given super key. Callers should select which super
    //key to be used. This is called when a key is super encrypted at its creation as well as at its
    //upgrade.
    fn encrypt_with_super_key(
        key_blob: &[u8],
        super_key: &SuperKey,
    ) -> Result<(Vec<u8>, BlobMetaData)> {
        let mut metadata = BlobMetaData::new();
        let (encrypted_key, iv, tag) = aes_gcm_encrypt(key_blob, &(super_key.key))
            .context("In encrypt_with_super_key: Failed to encrypt new super key.")?;
        metadata.add(BlobMetaEntry::Iv(iv));
        metadata.add(BlobMetaEntry::AeadTag(tag));
        metadata.add(BlobMetaEntry::EncryptedBy(EncryptedBy::KeyId(super_key.id)));
        Ok((encrypted_key, metadata))
    }

    /// Check if super encryption is required and if so, super-encrypt the key to be stored in
    /// the database.
    #[allow(clippy::clippy::too_many_arguments)]
    pub fn handle_super_encryption_on_key_init(
        &self,
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        domain: &Domain,
        key_parameters: &[KeyParameter],
        flags: Option<i32>,
        user_id: u32,
        key_blob: &[u8],
    ) -> Result<(Vec<u8>, BlobMetaData)> {
        match (*domain, Enforcements::super_encryption_required(key_parameters, flags)) {
            (Domain::APP, true) => {
                self.super_encrypt_on_key_init(db, legacy_migrator, user_id, &key_blob).context(
                    "In handle_super_encryption_on_key_init.
                         Failed to super encrypt the key.",
                )
            }
            _ => Ok((key_blob.to_vec(), BlobMetaData::new())),
        }
    }

    /// Check if a given key is super-encrypted, from its metadata. If so, unwrap the key using
    /// the relevant super key.
    pub fn unwrap_key_if_required<'a>(
        &self,
        metadata: &BlobMetaData,
        key_blob: &'a [u8],
    ) -> Result<KeyBlob<'a>> {
        if Self::key_super_encrypted(&metadata) {
            let unwrapped_key = self
                .unwrap_key(key_blob, metadata)
                .context("In unwrap_key_if_required. Error in unwrapping the key.")?;
            Ok(unwrapped_key)
        } else {
            Ok(KeyBlob::Ref(key_blob))
        }
    }

    /// Check if a given key needs re-super-encryption, from its KeyBlob type.
    /// If so, re-super-encrypt the key and return a new set of metadata,
    /// containing the new super encryption information.
    pub fn reencrypt_on_upgrade_if_required<'a>(
        key_blob_before_upgrade: &KeyBlob,
        key_after_upgrade: &'a [u8],
    ) -> Result<(KeyBlob<'a>, Option<BlobMetaData>)> {
        match key_blob_before_upgrade {
            KeyBlob::Sensitive(_, super_key) => {
                let (key, metadata) = Self::encrypt_with_super_key(key_after_upgrade, super_key)
                    .context(concat!(
                        "In reencrypt_on_upgrade_if_required. ",
                        "Failed to re-super-encrypt key on key upgrade."
                    ))?;
                Ok((KeyBlob::NonSensitive(key), Some(metadata)))
            }
            _ => Ok((KeyBlob::Ref(key_after_upgrade), None)),
        }
    }

    // Helper function to decide if a key is super encrypted, given metadata.
    fn key_super_encrypted(metadata: &BlobMetaData) -> bool {
        if let Some(&EncryptedBy::KeyId(_)) = metadata.encrypted_by() {
            return true;
        }
        false
    }
}

/// This enum represents different states of the user's life cycle in the device.
/// For now, only three states are defined. More states may be added later.
pub enum UserState {
    // The user has registered LSKF and has unlocked the device by entering PIN/Password,
    // and hence the per-boot super key is available in the cache.
    LskfUnlocked(SuperKey),
    // The user has registered LSKF, but has not unlocked the device using password, after reboot.
    // Hence the per-boot super-key(s) is not available in the cache.
    // However, the encrypted super key is available in the database.
    LskfLocked,
    // There's no user in the device for the given user id, or the user with the user id has not
    // setup LSKF.
    Uninitialized,
}

impl UserState {
    pub fn get(
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        skm: &SuperKeyManager,
        user_id: u32,
    ) -> Result<UserState> {
        match skm.get_per_boot_key_by_user_id(user_id) {
            Some(super_key) => Ok(UserState::LskfUnlocked(super_key)),
            None => {
                //Check if a super key exists in the database or legacy database.
                //If so, return locked user state.
                if SuperKeyManager::super_key_exists_in_db_for_user(db, legacy_migrator, user_id)
                    .context("In get.")?
                {
                    Ok(UserState::LskfLocked)
                } else {
                    Ok(UserState::Uninitialized)
                }
            }
        }
    }

    /// Queries user state when serving password change requests.
    pub fn get_with_password_changed(
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        skm: &SuperKeyManager,
        user_id: u32,
        password: Option<&[u8]>,
    ) -> Result<UserState> {
        match skm.get_per_boot_key_by_user_id(user_id) {
            Some(super_key) => {
                if password.is_none() {
                    //transitioning to swiping, delete only the super key in database and cache, and
                    //super-encrypted keys in database (and in KM)
                    Self::reset_user(db, skm, legacy_migrator, user_id, true).context(
                        "In get_with_password_changed: Trying to delete keys from the db.",
                    )?;
                    //Lskf is now removed in Keystore
                    Ok(UserState::Uninitialized)
                } else {
                    //Keystore won't be notified when changing to a new password when LSKF is
                    //already setup. Therefore, ideally this path wouldn't be reached.
                    Ok(UserState::LskfUnlocked(super_key))
                }
            }
            None => {
                //Check if a super key exists in the database or legacy database.
                //If so, return LskfLocked state.
                //Otherwise, i) if the password is provided, initialize the super key and return
                //LskfUnlocked state ii) if password is not provided, return Uninitialized state.
                skm.check_and_initialize_super_key(db, legacy_migrator, user_id, password)
            }
        }
    }

    /// Queries user state when serving password unlock requests.
    pub fn get_with_password_unlock(
        db: &mut KeystoreDB,
        legacy_migrator: &LegacyMigrator,
        skm: &SuperKeyManager,
        user_id: u32,
        password: &[u8],
    ) -> Result<UserState> {
        match skm.get_per_boot_key_by_user_id(user_id) {
            Some(super_key) => {
                log::info!("In get_with_password_unlock. Trying to unlock when already unlocked.");
                Ok(UserState::LskfUnlocked(super_key))
            }
            None => {
                //Check if a super key exists in the database or legacy database.
                //If not, return Uninitialized state.
                //Otherwise, try to unlock the super key and if successful,
                //return LskfUnlocked state
                skm.check_and_unlock_super_key(db, legacy_migrator, user_id, password)
                    .context("In get_with_password_unlock. Failed to unlock super key.")
            }
        }
    }

    /// Delete all the keys created on behalf of the user.
    /// If 'keep_non_super_encrypted_keys' is set to true, delete only the super key and super
    /// encrypted keys.
    pub fn reset_user(
        db: &mut KeystoreDB,
        skm: &SuperKeyManager,
        legacy_migrator: &LegacyMigrator,
        user_id: u32,
        keep_non_super_encrypted_keys: bool,
    ) -> Result<()> {
        // mark keys created on behalf of the user as unreferenced.
        legacy_migrator
            .bulk_delete_user(user_id, keep_non_super_encrypted_keys)
            .context("In reset_user: Trying to delete legacy keys.")?;
        db.unbind_keys_for_user(user_id as u32, keep_non_super_encrypted_keys)
            .context("In reset user. Error in unbinding keys.")?;

        //delete super key in cache, if exists
        skm.forget_all_keys_for_user(user_id as u32);
        Ok(())
    }
}

/// This enum represents three states a KeyMint Blob can be in, w.r.t super encryption.
/// `Sensitive` holds the non encrypted key and a reference to its super key.
/// `NonSensitive` holds a non encrypted key that is never supposed to be encrypted.
/// `Ref` holds a reference to a key blob when it does not need to be modified if its
/// life time allows it.
pub enum KeyBlob<'a> {
    Sensitive(ZVec, SuperKey),
    NonSensitive(Vec<u8>),
    Ref(&'a [u8]),
}

/// Deref returns a reference to the key material in any variant.
impl<'a> Deref for KeyBlob<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Sensitive(key, _) => &key,
            Self::NonSensitive(key) => &key,
            Self::Ref(key) => key,
        }
    }
}
