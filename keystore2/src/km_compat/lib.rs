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

// TODO: Once this is stable, remove this and document everything public.
#![allow(missing_docs)]

extern "C" {
    fn addKeyMintDeviceService() -> i32;
}

pub fn add_keymint_device_service() -> i32 {
    unsafe { addKeyMintDeviceService() }
}

#[cfg(test)]
mod tests {

    use super::*;
    use android_hardware_security_keymint::aidl::android::hardware::security::keymint::{
        Algorithm::Algorithm, BeginResult::BeginResult, BlockMode::BlockMode, Digest::Digest,
        ErrorCode::ErrorCode, HardwareAuthToken::HardwareAuthToken, IKeyMintDevice::IKeyMintDevice,
        KeyCreationResult::KeyCreationResult, KeyFormat::KeyFormat, KeyParameter::KeyParameter,
        KeyParameterArray::KeyParameterArray, KeyParameterValue::KeyParameterValue,
        KeyPurpose::KeyPurpose, PaddingMode::PaddingMode, SecurityLevel::SecurityLevel, Tag::Tag,
    };
    use android_hardware_security_keymint::binder;
    use android_security_compat::aidl::android::security::compat::IKeystoreCompatService::IKeystoreCompatService;

    static COMPAT_NAME: &str = "android.security.compat";

    fn get_device() -> Option<Box<dyn IKeyMintDevice>> {
        add_keymint_device_service();
        let compat_service: Box<dyn IKeystoreCompatService> =
            binder::get_interface(COMPAT_NAME).ok()?;
        compat_service.getKeyMintDevice(SecurityLevel::TRUSTED_ENVIRONMENT).ok()
    }

    macro_rules! get_device_or_skip_test {
        () => {
            match get_device() {
                Some(dev) => dev,
                None => return,
            }
        };
    }

    #[test]
    fn test_get_hardware_info() {
        let legacy = get_device_or_skip_test!();
        let hinfo = legacy.getHardwareInfo();
        assert!(hinfo.is_ok());
    }

    #[test]
    fn test_add_rng_entropy() {
        let legacy = get_device_or_skip_test!();
        let result = legacy.addRngEntropy(&[42; 16]);
        assert!(result.is_ok(), "{:?}", result);
    }

    // TODO: If I only need the key itself, don't return the other things.
    fn generate_key(legacy: &dyn IKeyMintDevice, kps: Vec<KeyParameter>) -> KeyCreationResult {
        let creation_result = legacy.generateKey(&kps).expect("Failed to generate key");
        assert_ne!(creation_result.keyBlob.len(), 0);
        creation_result
    }

    fn generate_rsa_key(legacy: &dyn IKeyMintDevice, encrypt: bool, attest: bool) -> Vec<u8> {
        let mut kps = vec![
            KeyParameter {
                tag: Tag::ALGORITHM,
                value: KeyParameterValue::Algorithm(Algorithm::RSA),
            },
            KeyParameter { tag: Tag::KEY_SIZE, value: KeyParameterValue::Integer(2048) },
            KeyParameter {
                tag: Tag::RSA_PUBLIC_EXPONENT,
                value: KeyParameterValue::LongInteger(65537),
            },
            KeyParameter { tag: Tag::DIGEST, value: KeyParameterValue::Digest(Digest::SHA_2_256) },
            KeyParameter {
                tag: Tag::PADDING,
                value: KeyParameterValue::PaddingMode(PaddingMode::RSA_PSS),
            },
            KeyParameter { tag: Tag::NO_AUTH_REQUIRED, value: KeyParameterValue::BoolValue(true) },
            KeyParameter {
                tag: Tag::PURPOSE,
                value: KeyParameterValue::KeyPurpose(KeyPurpose::SIGN),
            },
        ];
        if encrypt {
            kps.push(KeyParameter {
                tag: Tag::PURPOSE,
                value: KeyParameterValue::KeyPurpose(KeyPurpose::ENCRYPT),
            });
        }
        if attest {
            kps.push(KeyParameter {
                tag: Tag::ATTESTATION_CHALLENGE,
                value: KeyParameterValue::Blob(vec![42; 8]),
            });
            kps.push(KeyParameter {
                tag: Tag::ATTESTATION_APPLICATION_ID,
                value: KeyParameterValue::Blob(vec![42; 8]),
            });
        }
        let creation_result = generate_key(legacy, kps);
        if attest {
            // TODO: Will this always be greater than 1?
            assert!(creation_result.certificateChain.len() > 1);
        } else {
            assert_eq!(creation_result.certificateChain.len(), 1);
        }
        creation_result.keyBlob
    }

    #[test]
    fn test_generate_key_no_encrypt() {
        let legacy = get_device_or_skip_test!();
        generate_rsa_key(legacy.as_ref(), false, false);
    }

    #[test]
    fn test_generate_key_encrypt() {
        let legacy = get_device_or_skip_test!();
        generate_rsa_key(legacy.as_ref(), true, false);
    }

    #[test]
    fn test_generate_key_attested() {
        let legacy = get_device_or_skip_test!();
        generate_rsa_key(legacy.as_ref(), false, true);
    }

    #[test]
    fn test_import_key() {
        let legacy = get_device_or_skip_test!();
        let kps = [KeyParameter {
            tag: Tag::ALGORITHM,
            value: KeyParameterValue::Algorithm(Algorithm::AES),
        }];
        let kf = KeyFormat::RAW;
        let kd = [0; 16];
        let creation_result = legacy.importKey(&kps, kf, &kd).expect("Failed to import key");
        assert_ne!(creation_result.keyBlob.len(), 0);
        assert_eq!(creation_result.certificateChain.len(), 0);
    }

    #[test]
    fn test_import_wrapped_key() {
        let legacy = get_device_or_skip_test!();
        let result = legacy.importWrappedKey(&[], &[], &[], &[], 0, 0);
        // For this test we only care that there was no crash.
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_upgrade_key() {
        let legacy = get_device_or_skip_test!();
        let blob = generate_rsa_key(legacy.as_ref(), false, false);
        let result = legacy.upgradeKey(&blob, &[]);
        // For this test we only care that there was no crash.
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_delete_key() {
        let legacy = get_device_or_skip_test!();
        let blob = generate_rsa_key(legacy.as_ref(), false, false);
        let result = legacy.deleteKey(&blob);
        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn test_delete_all_keys() {
        let legacy = get_device_or_skip_test!();
        let result = legacy.deleteAllKeys();
        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn test_destroy_attestation_ids() {
        let legacy = get_device_or_skip_test!();
        let result = legacy.destroyAttestationIds();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().service_specific_error(), ErrorCode::UNIMPLEMENTED.0,);
    }

    fn generate_aes_key(legacy: &dyn IKeyMintDevice) -> Vec<u8> {
        let kps = vec![
            KeyParameter {
                tag: Tag::ALGORITHM,
                value: KeyParameterValue::Algorithm(Algorithm::AES),
            },
            KeyParameter { tag: Tag::KEY_SIZE, value: KeyParameterValue::Integer(128) },
            KeyParameter {
                tag: Tag::BLOCK_MODE,
                value: KeyParameterValue::BlockMode(BlockMode::CBC),
            },
            KeyParameter {
                tag: Tag::PADDING,
                value: KeyParameterValue::PaddingMode(PaddingMode::NONE),
            },
            KeyParameter { tag: Tag::NO_AUTH_REQUIRED, value: KeyParameterValue::BoolValue(true) },
            KeyParameter {
                tag: Tag::PURPOSE,
                value: KeyParameterValue::KeyPurpose(KeyPurpose::ENCRYPT),
            },
            KeyParameter {
                tag: Tag::PURPOSE,
                value: KeyParameterValue::KeyPurpose(KeyPurpose::DECRYPT),
            },
        ];
        let creation_result = generate_key(legacy, kps);
        assert_eq!(creation_result.certificateChain.len(), 0);
        creation_result.keyBlob
    }

    fn begin(
        legacy: &dyn IKeyMintDevice,
        blob: &[u8],
        purpose: KeyPurpose,
        extra_params: Option<Vec<KeyParameter>>,
    ) -> BeginResult {
        let mut kps = vec![
            KeyParameter {
                tag: Tag::BLOCK_MODE,
                value: KeyParameterValue::BlockMode(BlockMode::CBC),
            },
            KeyParameter {
                tag: Tag::PADDING,
                value: KeyParameterValue::PaddingMode(PaddingMode::NONE),
            },
        ];
        if let Some(mut extras) = extra_params {
            kps.append(&mut extras);
        }
        let result = legacy.begin(purpose, &blob, &kps, &HardwareAuthToken::default());
        assert!(result.is_ok(), "{:?}", result);
        result.unwrap()
    }

    #[test]
    fn test_begin_abort() {
        let legacy = get_device_or_skip_test!();
        let blob = generate_aes_key(legacy.as_ref());
        let begin_result = begin(legacy.as_ref(), &blob, KeyPurpose::ENCRYPT, None);
        let operation = begin_result.operation.unwrap();
        let result = operation.abort();
        assert!(result.is_ok(), "{:?}", result);
        let result = operation.abort();
        assert!(result.is_err());
    }

    #[test]
    fn test_begin_update_finish() {
        let legacy = get_device_or_skip_test!();
        let blob = generate_aes_key(legacy.as_ref());

        let begin_result = begin(legacy.as_ref(), &blob, KeyPurpose::ENCRYPT, None);
        let operation = begin_result.operation.unwrap();
        let params = KeyParameterArray {
            params: vec![KeyParameter {
                tag: Tag::ASSOCIATED_DATA,
                value: KeyParameterValue::Blob(b"foobar".to_vec()),
            }],
        };
        let message = [42; 128];
        let mut out_params = None;
        let result =
            operation.finish(Some(&params), Some(&message), None, None, None, &mut out_params);
        assert!(result.is_ok(), "{:?}", result);
        let ciphertext = result.unwrap();
        assert!(!ciphertext.is_empty());
        assert!(out_params.is_some());

        let begin_result =
            begin(legacy.as_ref(), &blob, KeyPurpose::DECRYPT, Some(begin_result.params));
        let operation = begin_result.operation.unwrap();
        let mut out_params = None;
        let mut output = None;
        let result = operation.update(
            Some(&params),
            Some(&ciphertext),
            None,
            None,
            &mut out_params,
            &mut output,
        );
        assert!(result.is_ok(), "{:?}", result);
        assert_eq!(result.unwrap(), message.len() as i32);
        assert!(output.is_some());
        assert_eq!(output.unwrap().data, message.to_vec());
        let result = operation.finish(Some(&params), None, None, None, None, &mut out_params);
        assert!(result.is_ok(), "{:?}", result);
        assert!(out_params.is_some());
    }

    #[test]
    fn test_secure_clock() {
        add_keymint_device_service();
        let compat_service: Box<dyn IKeystoreCompatService> =
            binder::get_interface(COMPAT_NAME).unwrap();
        let secure_clock = compat_service.getSecureClock().unwrap();

        let challenge = 42;
        let result = secure_clock.generateTimeStamp(challenge);
        assert!(result.is_ok(), "{:?}", result);
        let result = result.unwrap();
        assert_eq!(result.challenge, challenge);
        assert_eq!(result.mac.len(), 32);
    }

    #[test]
    fn test_shared_secret() {
        add_keymint_device_service();
        let compat_service: Box<dyn IKeystoreCompatService> =
            binder::get_interface(COMPAT_NAME).unwrap();
        let shared_secret =
            compat_service.getSharedSecret(SecurityLevel::TRUSTED_ENVIRONMENT).unwrap();

        let result = shared_secret.getSharedSecretParameters();
        assert!(result.is_ok(), "{:?}", result);
        let params = result.unwrap();

        let result = shared_secret.computeSharedSecret(&[params]);
        assert!(result.is_ok(), "{:?}", result);
        assert_ne!(result.unwrap().len(), 0);
    }
}
