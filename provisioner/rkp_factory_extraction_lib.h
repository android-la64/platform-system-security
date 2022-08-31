/*
 * Copyright 2022 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#include <aidl/android/hardware/security/keymint/IRemotelyProvisionedComponent.h>
#include <android/binder_manager.h>
#include <cppbor.h>
#include <keymaster/cppcose/cppcose.h>

#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <vector>

constexpr size_t kChallengeSize = 16;

// Contains the result of CSR generation, bundling up the result (on success)
// with an error message (on failure).
struct CsrResult {
    std::unique_ptr<cppbor::Array> csr;
    std::optional<std::string> errMsg;
};

// Return `buffer` encoded as a base64 string.
std::string toBase64(const std::vector<uint8_t>& buffer);

// Generate a random challenge containing `kChallengeSize` bytes.
std::vector<uint8_t> generateChallenge();

// Get a certificate signing request for the given IRemotelyProvisionedComponent.
// On error, the csr Array is null, and the string field contains a description of
// what went wrong.
CsrResult getCsr(std::string_view componentName,
                 aidl::android::hardware::security::keymint::IRemotelyProvisionedComponent* irpc);