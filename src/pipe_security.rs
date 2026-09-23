//! Windows named-pipe security descriptors.
//!
//! Local MCP approval/activity IPC must be readable and writable only by the
//! current Windows user. The Windows default named-pipe descriptor grants broad
//! principals (including Everyone/Anonymous), so pipe creation supplies an
//! explicit allowlist DACL instead.

#[cfg(test)]
use std::ffi::c_void;
use std::ptr;
use std::slice;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
#[cfg(test)]
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
#[cfg(test)]
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
#[cfg(test)]
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
#[cfg(test)]
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Access mask required for Local MCP approval/activity IPC.
///
/// Covers server create (including additional instances) and client open with
/// read/write, plus owner management rights.
#[cfg(test)]
pub const PIPE_USER_ACCESS_MASK: u32 = FILE_ALL_ACCESS;

/// Owned absolute security descriptor with a current-user-only DACL.
///
/// The descriptor is allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`
/// and freed with `LocalFree`. Keep this alive for the entire pipe-create call.
pub struct CurrentUserPipeSecurity {
    sd: PSECURITY_DESCRIPTOR,
}

impl CurrentUserPipeSecurity {
    /// Builds a protected DACL granting only the current Windows user.
    ///
    /// SYSTEM and Administrators are intentionally omitted: Local MCP IPC is
    /// same-user only. Everyone/Anonymous receive no grant.
    pub fn new() -> Result<Self> {
        let sid =
            current_user_sid_string().context("failed to resolve the current Windows user SID")?;
        // SDDL ACE form: type;flags;rights;object_guid;inherit_object_guid;trustee
        // `FA` is FILE_ALL_ACCESS. No Everyone/Anonymous/Authenticated Users ACE.
        let sddl = format!("O:{sid}G:{sid}D:P(A;;FA;;;{sid})");
        Self::from_sddl(&sddl)
    }

    fn from_sddl(sddl: &str) -> Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut sd_size = 0u32;
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                &mut sd_size,
            )
        };
        if ok == 0 {
            bail!(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW failed (os error {})",
                unsafe { GetLastError() }
            );
        }
        if sd.is_null() {
            bail!("SDDL conversion returned a null security descriptor");
        }
        Ok(Self { sd })
    }

    /// SECURITY_ATTRIBUTES view for `CreateNamedPipeW` / Tokio's raw create API.
    ///
    /// The returned struct borrows `self`; do not use it after `self` drops.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd,
            bInheritHandle: 0,
        }
    }
}

impl Drop for CurrentUserPipeSecurity {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            unsafe {
                LocalFree(self.sd.cast());
            }
            self.sd = ptr::null_mut();
        }
    }
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Resolves the process token user SID to its SDDL string form (e.g. `S-1-5-21-...`).
fn current_user_sid_string() -> Result<String> {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            bail!("OpenProcessToken failed (os error {})", GetLastError());
        }
        let token = HandleGuard(token);

        let mut needed = 0u32;
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            bail!(
                "GetTokenInformation(TokenUser) size query failed (os error {})",
                GetLastError()
            );
        }
        let mut buf = vec![0u8; needed as usize];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            bail!(
                "GetTokenInformation(TokenUser) failed (os error {})",
                GetLastError()
            );
        }
        let token_user = &*buf.as_ptr().cast::<TOKEN_USER>();
        if token_user.User.Sid.is_null() {
            bail!("TokenUser returned a null SID");
        }

        sid_to_string(token_user.User.Sid)
    }
}

/// Wide C string → `String`, then the caller frees the original buffer.
unsafe fn wide_ptr_to_string(ptr: *const u16) -> Result<String> {
    unsafe {
        if ptr.is_null() {
            bail!("wide string pointer is null");
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(slice::from_raw_parts(ptr, len)))
    }
}

/// One access-allow or access-deny ACE principal and mask.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct AceSummary {
    pub sid_string: String,
    pub mask: u32,
    pub allowed: bool,
}

/// Reads the DACL owner SID and ACE list from an absolute security descriptor.
#[cfg(test)]
pub fn inspect_security_descriptor(sd: PSECURITY_DESCRIPTOR) -> Result<(String, Vec<AceSummary>)> {
    if sd.is_null() {
        bail!("security descriptor is null");
    }
    unsafe {
        let mut owner: PSID = ptr::null_mut();
        let mut owner_defaulted = 0;
        if GetSecurityDescriptorOwner(sd, &mut owner, &mut owner_defaulted) == 0 {
            bail!(
                "GetSecurityDescriptorOwner failed (os error {})",
                GetLastError()
            );
        }
        let owner_sid = if owner.is_null() {
            String::from("<null>")
        } else {
            sid_to_string(owner)?
        };

        let mut dacl_present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut dacl_defaulted = 0;
        if GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted) == 0 {
            bail!(
                "GetSecurityDescriptorDacl failed (os error {})",
                GetLastError()
            );
        }
        if dacl_present == 0 || dacl.is_null() {
            bail!("security descriptor has no DACL (null DACL allows everyone)");
        }

        let acl = &*dacl;
        let ace_count = acl.AceCount as u32;
        let mut aces = Vec::with_capacity(ace_count as usize);
        for index in 0..ace_count {
            let mut ace_ptr: *mut c_void = ptr::null_mut();
            if GetAce(dacl, index, &mut ace_ptr) == 0 {
                bail!("GetAce({index}) failed (os error {})", GetLastError());
            }
            if ace_ptr.is_null() {
                bail!("GetAce({index}) returned null");
            }
            let header = &*(ace_ptr.cast::<windows_sys::Win32::Security::ACE_HEADER>());
            let (allowed, mask, sid) = match header.AceType as u32 {
                ACCESS_ALLOWED_ACE_TYPE => {
                    let ace = &*(ace_ptr.cast::<ACCESS_ALLOWED_ACE>());
                    let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast::<c_void>();
                    (true, ace.Mask, sid)
                }
                ACCESS_DENIED_ACE_TYPE => {
                    let ace = &*(ace_ptr.cast::<ACCESS_ALLOWED_ACE>());
                    let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast::<c_void>();
                    (false, ace.Mask, sid)
                }
                other => bail!("unsupported ACE type {other} at index {index}"),
            };
            aces.push(AceSummary {
                sid_string: sid_to_string(sid)?,
                mask,
                allowed,
            });
        }
        Ok((owner_sid, aces))
    }
}

/// Converts a Windows SID to its SDDL string form, freeing the API buffer.
unsafe fn sid_to_string(sid: PSID) -> Result<String> {
    unsafe {
        if sid.is_null() {
            bail!("SID is null");
        }
        let mut sid_str: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut sid_str) == 0 {
            bail!(
                "ConvertSidToStringSidW failed (os error {})",
                GetLastError()
            );
        }
        let text = wide_ptr_to_string(sid_str);
        LocalFree(sid_str.cast());
        text
    }
}

/// The current user SID as an SDDL string (for tests that compare principals).
#[cfg(test)]
pub fn current_user_sid() -> Result<String> {
    current_user_sid_string()
}

/// Reads owner + DACL ACEs from an open kernel handle (named-pipe server).
#[cfg(test)]
pub fn inspect_handle_security(handle: HANDLE) -> Result<(String, Vec<AceSummary>)> {
    unsafe {
        let mut owner: PSID = ptr::null_mut();
        let mut group: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut sacl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            &mut group,
            &mut dacl,
            &mut sacl,
            &mut sd,
        );
        if status != 0 {
            bail!("GetSecurityInfo failed (win32 status {status})");
        }
        if sd.is_null() {
            bail!("GetSecurityInfo returned a null descriptor");
        }
        let result = inspect_security_descriptor(sd);
        LocalFree(sd.cast());
        result
    }
}

/// Well-known broad principals that must not hold pipe access.
#[cfg(test)]
pub const FORBIDDEN_BROAD_SID_PREFIXES: &[&str] = &[
    // Everyone / World
    "S-1-1-0",
    // Anonymous / Anonymous Logon
    "S-1-5-7",
    // Authenticated Users
    "S-1-5-11",
    // This Organization
    "S-1-5-15",
    // Builtin\Users — not current-user-only
    "S-1-5-32-547",
];

/// Returns true when `sid` is a broad principal that must not access the pipe.
#[cfg(test)]
pub fn is_broad_principal(sid: &str) -> bool {
    FORBIDDEN_BROAD_SID_PREFIXES
        .iter()
        .any(|prefix| sid == *prefix || sid.starts_with(&format!("{prefix}-")))
}

/// Mechanical check: owner is current user; every allow ACE is current-user only;
/// no allow ACE for broad principals; current user has `PIPE_USER_ACCESS_MASK`.
#[cfg(test)]
pub fn assert_current_user_only(
    owner_sid: &str,
    aces: &[AceSummary],
    current_user: &str,
) -> Result<()> {
    if owner_sid != current_user {
        bail!("pipe owner must be the current user SID (owner is a different principal)");
    }
    let mut current_user_access = 0u32;
    for ace in aces {
        if is_broad_principal(&ace.sid_string) {
            bail!(
                "broad principal is present in the pipe DACL (sid class = {})",
                broad_sid_class(&ace.sid_string)
            );
        }
        if ace.sid_string != current_user {
            bail!(
                "non-current-user principal present in the pipe DACL (sid class = {})",
                sid_class(&ace.sid_string)
            );
        }
        if ace.allowed {
            current_user_access |= ace.mask;
        } else {
            current_user_access &= !ace.mask;
        }
    }
    let missing = PIPE_USER_ACCESS_MASK & !current_user_access;
    if missing != 0 {
        bail!("current user is missing required pipe access (missing mask = {missing:#x})");
    }
    Ok(())
}

#[cfg(test)]
fn sid_class(sid: &str) -> String {
    if sid.starts_with("S-1-5-21-") {
        String::from("account-or-domain-sid")
    } else {
        format!("sid({sid})")
    }
}

#[cfg(test)]
fn broad_sid_class(sid: &str) -> String {
    match sid {
        "S-1-1-0" => String::from("Everyone"),
        "S-1-5-7" => String::from("Anonymous"),
        "S-1-5-11" => String::from("AuthenticatedUsers"),
        "S-1-5-15" => String::from("ThisOrganization"),
        "S-1-5-32-547" => String::from("BuiltinUsers"),
        _ => sid_class(sid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_current_user_only_excludes_broad_principals() {
        let user = current_user_sid().expect("current user SID");
        let security =
            CurrentUserPipeSecurity::from_sddl(&format!("O:{user}G:{user}D:P(A;;FA;;;{user})"))
                .expect("build SD");
        let attrs = security.attributes();
        assert!(!attrs.lpSecurityDescriptor.is_null());
        assert_eq!(
            attrs.nLength as usize,
            std::mem::size_of::<SECURITY_ATTRIBUTES>()
        );

        let (owner, aces) =
            inspect_security_descriptor(attrs.lpSecurityDescriptor).expect("inspect SD");
        assert_current_user_only(&owner, &aces, &user).expect("current-user-only DACL");
    }

    #[test]
    fn everyones_ace_is_rejected() {
        let user = current_user_sid().expect("current user SID");
        let aces = vec![AceSummary {
            sid_string: "S-1-1-0".into(),
            mask: FILE_ALL_ACCESS,
            allowed: true,
        }];
        let err = assert_current_user_only(&user, &aces, &user).unwrap_err();
        assert!(err.to_string().contains("Everyone") || err.to_string().contains("broad"));
    }

    #[test]
    fn anonymous_ace_is_rejected() {
        let user = current_user_sid().expect("current user SID");
        let aces = vec![AceSummary {
            sid_string: "S-1-5-7".into(),
            mask: FILE_ALL_ACCESS,
            allowed: true,
        }];
        let err = assert_current_user_only(&user, &aces, &user).unwrap_err();
        assert!(err.to_string().contains("Anonymous") || err.to_string().contains("broad"));
    }

    #[test]
    fn unrelated_user_ace_is_rejected() {
        let user = current_user_sid().expect("current user SID");
        let unrelated = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        let aces = vec![AceSummary {
            sid_string: unrelated.into(),
            mask: FILE_ALL_ACCESS,
            allowed: true,
        }];
        assert!(assert_current_user_only(&user, &aces, &user).is_err());
    }

    #[test]
    fn insufficient_mask_is_rejected() {
        let user = current_user_sid().expect("current user SID");
        let aces = vec![AceSummary {
            sid_string: user.clone(),
            mask: 0x1,
            allowed: true,
        }];
        assert!(assert_current_user_only(&user, &aces, &user).is_err());
    }

    #[test]
    fn owner_mismatch_is_rejected() {
        let user = current_user_sid().expect("current user SID");
        let other = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        let aces = vec![AceSummary {
            sid_string: user.clone(),
            mask: FILE_ALL_ACCESS,
            allowed: true,
        }];
        assert!(assert_current_user_only(other, &aces, &user).is_err());
    }
}
