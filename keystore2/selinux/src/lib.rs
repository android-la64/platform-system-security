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

// #![allow(missing_docs)]

//! This crate provides some safe wrappers around the libselinux API. It is currently limited
//! to the API surface that Keystore 2.0 requires to perform permission checks against
//! the SEPolicy. Notably, it provides wrappers for:
//!  * getcon
//!  * selinux_check_access
//!  * selabel_lookup for the keystore2_key backend.
//! And it provides an owning wrapper around context strings `Context`.

use std::ffi::{CStr, CString};
use std::fmt;
use std::io;
pub use std::ops::Deref;
use std::os::raw::c_char;
use std::ptr;
use std::sync;

use selinux_bindgen as selinux;

use anyhow::Context as AnyhowContext;
use anyhow::{anyhow, Result};

use selinux::SELABEL_CTX_ANDROID_KEYSTORE2_KEY;
use selinux::SELINUX_CB_LOG;

static SELINUX_LOG_INIT: sync::Once = sync::Once::new();

fn redirect_selinux_logs_to_logcat() {
    // `selinux_set_callback` assigned the static lifetime function pointer
    // `selinux_log_callback` to a static lifetime variable.
    let cb = selinux::selinux_callback { func_log: Some(selinux::selinux_log_callback) };
    unsafe {
        selinux::selinux_set_callback(SELINUX_CB_LOG as i32, cb);
    }
}

// This function must be called before any entrypoint into lib selinux.
// Or leave a comment reasoning why calling this macro is not necessary
// for a given entry point.
fn init_logger_once() {
    SELINUX_LOG_INIT.call_once(redirect_selinux_logs_to_logcat)
}

/// Selinux Error code.
#[derive(thiserror::Error, Debug, PartialEq)]
pub enum Error {
    /// Indicates that an access check yielded no access.
    #[error("Permission Denied")]
    PermissionDenied,
    /// Indicates an unexpected system error. Nested string provides some details.
    #[error("Selinux SystemError: {0}")]
    SystemError(String),
}

impl Error {
    /// Constructs a `PermissionDenied` error.
    pub fn perm() -> Self {
        Error::PermissionDenied
    }
    fn sys<T: Into<String>>(s: T) -> Self {
        Error::SystemError(s.into())
    }
}

/// Context represents an SELinux context string. It can take ownership of a raw
/// s-string as allocated by `getcon` or `selabel_lookup`. In this case it uses
/// `freecon` to free the resources when dropped. In its second variant it stores
/// an `std::ffi::CString` that can be initialized from a Rust string slice.
pub enum Context {
    /// Wraps a raw context c-string as returned by libselinux.
    Raw(*mut ::std::os::raw::c_char),
    /// Stores a context string as `std::ffi::CString`.
    CString(CString),
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", (**self).to_str().unwrap_or("Invalid context"))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Self::Raw(p) = self {
            // No need to initialize the logger here, because
            // `freecon` cannot run unless `Backend::lookup` or `getcon`
            // has run.
            unsafe { selinux::freecon(*p) };
        }
    }
}

impl Deref for Context {
    type Target = CStr;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Raw(p) => unsafe { CStr::from_ptr(*p) },
            Self::CString(cstr) => &cstr,
        }
    }
}

impl Context {
    /// Initializes the `Context::CString` variant from a Rust string slice.
    pub fn new(con: &str) -> Result<Self> {
        Ok(Self::CString(
            CString::new(con)
                .with_context(|| format!("Failed to create Context with \"{}\"", con))?,
        ))
    }
}

/// The backend trait provides a uniform interface to all libselinux context backends.
/// Currently, we only implement the KeystoreKeyBackend though.
pub trait Backend {
    /// Implementers use libselinux `selabel_lookup` to lookup the context for the given `key`.
    fn lookup(&self, key: &str) -> Result<Context>;
}

/// Keystore key backend takes onwnership of the SELinux context handle returned by
/// `selinux_android_keystore2_key_context_handle` and uses `selabel_close` to free
/// the handle when dropped.
/// It implements `Backend` to provide keystore_key label lookup functionality.
pub struct KeystoreKeyBackend {
    handle: *mut selinux::selabel_handle,
}

impl KeystoreKeyBackend {
    const BACKEND_TYPE: i32 = SELABEL_CTX_ANDROID_KEYSTORE2_KEY as i32;

    /// Creates a new instance representing an SELinux context handle as returned by
    /// `selinux_android_keystore2_key_context_handle`.
    pub fn new() -> Result<Self> {
        init_logger_once();
        let handle = unsafe { selinux::selinux_android_keystore2_key_context_handle() };
        if handle.is_null() {
            return Err(anyhow!(Error::sys("Failed to open KeystoreKeyBackend")));
        }
        Ok(KeystoreKeyBackend { handle })
    }
}

impl Drop for KeystoreKeyBackend {
    fn drop(&mut self) {
        // No need to initialize the logger here because it cannot be called unless
        // KeystoreKeyBackend::new has run.
        unsafe { selinux::selabel_close(self.handle) };
    }
}

impl Backend for KeystoreKeyBackend {
    fn lookup(&self, key: &str) -> Result<Context> {
        let mut con: *mut c_char = ptr::null_mut();
        let c_key = CString::new(key).with_context(|| {
            format!("selabel_lookup: Failed to convert key \"{}\" to CString.", key)
        })?;
        match unsafe {
            // No need to initialize the logger here because it cannot run unless
            // KeystoreKeyBackend::new has run.
            selinux::selabel_lookup(self.handle, &mut con, c_key.as_ptr(), Self::BACKEND_TYPE)
        } {
            0 => {
                if !con.is_null() {
                    Ok(Context::Raw(con))
                } else {
                    Err(anyhow!(Error::sys(format!(
                        "selabel_lookup returned a NULL context for key \"{}\"",
                        key
                    ))))
                }
            }
            _ => Err(anyhow!(io::Error::last_os_error()))
                .with_context(|| format!("selabel_lookup failed for key \"{}\"", key)),
        }
    }
}

/// Safe wrapper around libselinux `getcon`. It initializes the `Context::Raw` variant of the
/// returned `Context`.
///
/// ## Return
///  * Ok(Context::Raw()) if successful.
///  * Err(Error::sys()) if getcon succeeded but returned a NULL pointer.
///  * Err(io::Error::last_os_error()) if getcon failed.
pub fn getcon() -> Result<Context> {
    init_logger_once();
    let mut con: *mut c_char = ptr::null_mut();
    match unsafe { selinux::getcon(&mut con) } {
        0 => {
            if !con.is_null() {
                Ok(Context::Raw(con))
            } else {
                Err(anyhow!(Error::sys("getcon returned a NULL context")))
            }
        }
        _ => Err(anyhow!(io::Error::last_os_error())).context("getcon failed"),
    }
}

/// Safe wrapper around selinux_check_access.
///
/// ## Return
///  * Ok(()) iff the requested access was granted.
///  * Err(anyhow!(Error::perm()))) if the permission was denied.
///  * Err(anyhow!(ioError::last_os_error())) if any other error occurred while performing
///            the access check.
pub fn check_access(source: &Context, target: &Context, tclass: &str, perm: &str) -> Result<()> {
    init_logger_once();
    let c_tclass = CString::new(tclass).with_context(|| {
        format!("check_access: Failed to convert tclass \"{}\" to CString.", tclass)
    })?;
    let c_perm = CString::new(perm).with_context(|| {
        format!("check_access: Failed to convert perm \"{}\" to CString.", perm)
    })?;

    match unsafe {
        selinux::selinux_check_access(
            source.as_ptr(),
            target.as_ptr(),
            c_tclass.as_ptr(),
            c_perm.as_ptr(),
            ptr::null_mut(),
        )
    } {
        0 => Ok(()),
        _ => {
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::PermissionDenied => Err(anyhow!(Error::perm())),
                _ => Err(anyhow!(e)),
            }
            .with_context(|| {
                format!(
                    concat!(
                        "check_access: Failed with sctx: {} tctx: {}",
                        " with target class: \"{}\" perm: \"{}\""
                    ),
                    source, target, tclass, perm
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    /// The su_key namespace as defined in su.te and keystore_key_contexts of the
    /// SePolicy (system/sepolicy).
    static SU_KEY_NAMESPACE: &str = "0";
    /// The shell_key namespace as defined in shell.te and keystore_key_contexts of the
    /// SePolicy (system/sepolicy).
    static SHELL_KEY_NAMESPACE: &str = "1";

    fn check_context() -> Result<(Context, &'static str, bool)> {
        let context = getcon()?;
        match context.to_str().unwrap() {
            "u:r:su:s0" => Ok((context, SU_KEY_NAMESPACE, true)),
            "u:r:shell:s0" => Ok((context, SHELL_KEY_NAMESPACE, false)),
            c => Err(anyhow!(format!(
                "This test must be run as \"su\" or \"shell\". Current context: \"{}\"",
                c
            ))),
        }
    }

    #[test]
    fn test_getcon() -> Result<()> {
        check_context()?;
        Ok(())
    }

    #[test]
    fn test_label_lookup() -> Result<()> {
        let (_context, namespace, is_su) = check_context()?;
        let backend = crate::KeystoreKeyBackend::new()?;
        let context = backend.lookup(namespace)?;
        if is_su {
            assert_eq!(context.to_str(), Ok("u:object_r:su_key:s0"));
        } else {
            assert_eq!(context.to_str(), Ok("u:object_r:shell_key:s0"));
        }
        Ok(())
    }

    #[test]
    fn context_from_string() -> Result<()> {
        let tctx = Context::new("u:object_r:keystore:s0").unwrap();
        let sctx = Context::new("u:r:system_server:s0").unwrap();
        check_access(&sctx, &tctx, "keystore2_key", "use")?;
        Ok(())
    }

    mod perm {
        use super::super::*;
        use super::*;
        use anyhow::Result;

        /// check_key_perm(perm, privileged, priv_domain)
        /// `perm` is a permission of the keystore2_key class and `privileged` is a boolean
        /// indicating whether the permission is considered privileged.
        /// Privileged permissions are expeced to be denied to `shell` users but granted
        /// to the given priv_domain.
        macro_rules! check_key_perm {
            // "use" is a keyword and cannot be used as an identifier, but we must keep
            // the permission string intact. So we map the identifier name on use_ while using
            // the permission string "use". In all other cases we can simply use the stringified
            // identifier as permission string.
            (use, $privileged:expr) => {
                check_key_perm!(use_, $privileged, "use");
            };
            ($perm:ident, $privileged:expr) => {
                check_key_perm!($perm, $privileged, stringify!($perm));
            };
            ($perm:ident, $privileged:expr, $p_str:expr) => {
                #[test]
                fn $perm() -> Result<()> {
                    android_logger::init_once(
                        android_logger::Config::default()
                            .with_tag("keystore_selinux_tests")
                            .with_min_level(log::Level::Debug),
                    );
                    let scontext = Context::new("u:r:shell:s0")?;
                    let backend = KeystoreKeyBackend::new()?;
                    let tcontext = backend.lookup(SHELL_KEY_NAMESPACE)?;

                    if $privileged {
                        assert_eq!(
                            Some(&Error::perm()),
                            check_access(
                                &scontext,
                                &tcontext,
                                "keystore2_key",
                                $p_str
                            )
                            .err()
                            .unwrap()
                            .root_cause()
                            .downcast_ref::<Error>()
                        );
                    } else {
                        assert!(check_access(
                            &scontext,
                            &tcontext,
                            "keystore2_key",
                            $p_str
                        )
                        .is_ok());
                    }
                    Ok(())
                }
            };
        }

        check_key_perm!(manage_blob, true);
        check_key_perm!(delete, false);
        check_key_perm!(use_dev_id, true);
        check_key_perm!(req_forced_op, true);
        check_key_perm!(gen_unique_id, true);
        check_key_perm!(grant, true);
        check_key_perm!(get_info, false);
        check_key_perm!(list, false);
        check_key_perm!(rebind, false);
        check_key_perm!(update, false);
        check_key_perm!(use, false);

        macro_rules! check_keystore_perm {
            ($perm:ident) => {
                #[test]
                fn $perm() -> Result<()> {
                    let ks_context = Context::new("u:object_r:keystore:s0")?;
                    let priv_context = Context::new("u:r:system_server:s0")?;
                    let unpriv_context = Context::new("u:r:shell:s0")?;
                    assert!(check_access(
                        &priv_context,
                        &ks_context,
                        "keystore2",
                        stringify!($perm)
                    )
                    .is_ok());
                    assert_eq!(
                        Some(&Error::perm()),
                        check_access(&unpriv_context, &ks_context, "keystore2", stringify!($perm))
                            .err()
                            .unwrap()
                            .root_cause()
                            .downcast_ref::<Error>()
                    );
                    Ok(())
                }
            };
        }

        check_keystore_perm!(add_auth);
        check_keystore_perm!(clear_ns);
        check_keystore_perm!(get_state);
        check_keystore_perm!(lock);
        check_keystore_perm!(reset);
        check_keystore_perm!(unlock);
    }
}
