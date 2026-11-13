#!/usr/bin/env python3
"""H5.3.1 — create and verify a fixture root that only its owner can write.

Why this module exists
----------------------
The H5.3-0 harness refused to reuse a foreign root, refused reparse points and
hard links, and (after review round 5) created its root `0o700` on POSIX. On
Windows it returned early, because the H5.3 review assumed `%TEMP%` is per-user.
That assumption is false on ordinary machines: measured on the campaign host
(2026-11-14, Windows 11, `Get-Acl` on `C:\\Users\\<user>\\AppData\\Local\\Temp`),
the DACL granted `Modify` (mask `0x1301bf`) to two further local SIDs and to a
sandbox group, e.g.::

    O:...G:...D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)
      (A;OICI;0x1301bf;;;S-1-5-21-...-1002)
      (A;OICI;0x1301bf;;;S-1-5-21-...-1008)  -> <host>\\CodexSandboxUsers
      (A;OICI;0x1301bf;;;S-1-5-21-...)
      (A;OICI;FA;;;S-1-5-21-...-1001)        -> the owner

So a root created under `%TEMP%` with an inherited DACL is writable by another
OS user, who could replace a fixture file with a link between the harness's
hazard scan and its write (the exact race the round-5 reviewer kept open as
R1b). `FA` is `FILE_ALL_ACCESS`; `0x1301bf` is `Modify`.

What this module guarantees
---------------------------
* A *new* root is created with an explicit, protected, owner-only DACL
  (`SYSTEM`, `Administrators`, the creating user) instead of an inherited one, so
  another ordinary OS user holds no write right inside it and cannot create,
  replace, link or delete entries in it. The descriptor is supplied to
  `CreateDirectory` itself, so there is no create-then-harden window.
* The DACL is read back through a second API and verified. Anything the check
  cannot parse or prove — a `NULL` DACL, an object/callback ACE it does not
  model, an unexpected trustee, a missing protection flag — is a *refusal*, not
  a warning. This is the fail-closed stance the H5.3.1 brief requires.
* An *existing* root is verified the same way before it is reused, and refused if
  it is not provably owner-only.

What it does not guarantee
--------------------------
A process running as the *same* OS user (or as an administrator) can still write
inside the root regardless of its DACL. That same-user boundary is the product
constraint recorded as **C3 / R-1**; this module neither fixes nor hides it. It
also does not modify any ACL outside the root it just created.
"""

from __future__ import annotations

import ctypes
import os
import stat
import sys
from pathlib import Path

__all__ = [
    "PrivateRootError",
    "create_owner_only_dir",
    "assert_private_root",
    "assert_private_entry",
    "verify_private_tree",
    "assert_path_cannot_reenter",
    "restrict_file",
    "RunLease",
    "acquire_run_lease",
    "create_private_dir",
    "create_private_dir_exclusive",
    "trustee_report",
    "support_class",
]

#: Owner-only POSIX mode for a fixture root.
POSIX_OWNER_ONLY_MODE = 0o700


class PrivateRootError(RuntimeError):
    """The fixture root cannot be created, or cannot be proven private."""


# ---------------------------------------------------------------------------
# POSIX
# ---------------------------------------------------------------------------


def _posix_create(path: Path) -> None:
    # `mkdir(mode=...)` is atomic: the mode is already restrictive when the entry
    # appears, unlike `mkdir()` followed by `chmod()`.
    os.mkdir(path, POSIX_OWNER_ONLY_MODE)
    mode = stat.S_IMODE(path.lstat().st_mode)
    if mode != POSIX_OWNER_ONLY_MODE:
        # The process umask masked our mode (a 0700 request stays private under
        # any umask, but a hostile umask can strip the owner bits too). This is a
        # directory we just created, so tightening it is our own decision.
        os.chmod(path, POSIX_OWNER_ONLY_MODE)


def _posix_assert(path: Path, expect_directory: bool) -> None:
    info = path.lstat()
    if info.st_uid != os.geteuid():
        raise PrivateRootError(
            f"{path} is owned by uid {info.st_uid}, not by the running user "
            f"({os.geteuid()}); another user's root cannot be proven private, so "
            "it is refused"
        )
    mode = stat.S_IMODE(info.st_mode)
    if mode & 0o077:
        raise PrivateRootError(
            f"{path} is group- or world-accessible (mode {oct(mode)}); "
            "a shared fixture root cannot be proven private, so it is refused"
        )
    # A directory needs owner rwx; a regular file needs neither x nor, for a
    # marker written with the default mode (0644/0600), an owner execute bit.
    if expect_directory and mode & POSIX_OWNER_ONLY_MODE != POSIX_OWNER_ONLY_MODE:
        raise PrivateRootError(
            f"{path} is not owner-accessible (mode {oct(mode)}); "
            "the fixture root cannot be used"
        )


# ---------------------------------------------------------------------------
# Windows
# ---------------------------------------------------------------------------

# Well-known principals that are outside the ordinary-user threat model. They are
# trusted deliberately: an administrator can already read and write anything, so
# excluding them from the DACL would provide no additional protection. See
# docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md.
_TRUSTED_SIDS = frozenset(
    {
        "S-1-5-18",  # NT AUTHORITY\SYSTEM
        "S-1-5-32-544",  # BUILTIN\Administrators
        # NT SERVICE\TrustedInstaller: the owner of C:\ on a normal Windows host.
        # It administers the volume, so it is inside the trusted boundary.
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    }
)

#: `OWNER RIGHTS` (S-1-3-4) is a *dynamic* trustee: an ACE for it applies only to
#: the object's owner. It is equivalent to naming the owner, and it is what the OS
#: and CPython's own `os.mkdir(path, 0o700)` produce, so it is accepted - but only
#: after the descriptor's owner has been proven to be the current user.
_OWNER_RIGHTS_SID = "S-1-3-4"

# Rights that let a holder change the fixture's content, replace an entry with a
# link, or re-point the object. Read/execute-only trustees do not matter here.
_WRITE_RIGHTS = (
    0x00000002  # FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x00000004  # FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x00000010  # FILE_WRITE_EA
    | 0x00000040  # FILE_DELETE_CHILD
    | 0x00000100  # FILE_WRITE_ATTRIBUTES
    | 0x00010000  # DELETE
    | 0x00040000  # WRITE_DAC
    | 0x00080000  # WRITE_OWNER
    | 0x40000000  # GENERIC_WRITE
    | 0x10000000  # GENERIC_ALL
)

_DACL_SECURITY_INFORMATION = 0x00000004
_PROTECTED_DACL_SECURITY_INFORMATION = 0x80000000
_SE_DACL_PROTECTED = 0x1000
_SE_DACL_PRESENT = 0x0004
_SE_FILE_OBJECT = 1
_SDDL_REVISION_1 = 1

_ACCESS_ALLOWED_ACE_TYPE = 0x00
_ACCESS_DENIED_ACE_TYPE = 0x01
_ACCESS_ALLOWED_OBJECT_ACE_TYPE = 0x05
_ACCESS_DENIED_OBJECT_ACE_TYPE = 0x06
_ACCESS_ALLOWED_CALLBACK_ACE_TYPE = 0x09
_ACCESS_DENIED_CALLBACK_ACE_TYPE = 0x0A
_ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE = 0x0B
_ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE = 0x0C

_ACE_OBJECT_TYPE_PRESENT = 0x1
_ACE_INHERITED_OBJECT_TYPE_PRESENT = 0x2

_MASK_OFFSET = 4
_SID_OFFSET_PLAIN = 8
_OBJECT_INHERIT_ACE = 0x01
_CONTAINER_INHERIT_ACE = 0x02
_INHERITED_ACE = 0x10
_INHERIT_BOTH = _OBJECT_INHERIT_ACE | _CONTAINER_INHERIT_ACE
_INHERIT_ONLY_ACE = 0x08
_NO_PROPAGATE_INHERIT_ACE = 0x04
#: Rights a directory needs before this harness can create the fixture in it.
_CREATE_IN_DIRECTORY = 0x00000002 | 0x00000004  # FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY

#: Rights that let a trustee rename or delete an entry, or the component itself,
#: independently of the child's own DACL. Two routes exist on Windows and both
#: matter: `DELETE` on the object (a rename or delete of *that* component) and
#: `FILE_DELETE_CHILD` on its parent (a bypass of the child's DACL).
#: `WRITE_DAC` / `WRITE_OWNER` let a trustee grant itself either right first.
#: `Modify` (0x1301bf) contains `DELETE` but not `FILE_DELETE_CHILD`, so a
#: `Modify` grant on a shared temporary directory lets another account rename the
#: temporary directory itself - which is why such a base is now refused.
_REENTRY_RIGHTS = (
    0x00010000  # DELETE (on the object itself: rename/delete of that component)
    | 0x00000040  # FILE_DELETE_CHILD
    | 0x00040000  # WRITE_DAC
    | 0x00080000  # WRITE_OWNER
    | 0x10000000  # GENERIC_ALL
)


class _ACEHeader(ctypes.Structure):
    _fields_ = [
        ("AceType", ctypes.c_ubyte),
        ("AceFlags", ctypes.c_ubyte),
        ("AceSize", ctypes.c_ushort),
    ]


class _SecurityAttributes(ctypes.Structure):
    _fields_ = [
        ("nLength", ctypes.c_ulong),
        ("lpSecurityDescriptor", ctypes.c_void_p),
        ("bInheritHandle", ctypes.c_int),
    ]


def _windows_only() -> bool:
    return os.name == "nt"


def _load_windows_api():
    """Bind the few advapi32/kernel32 entry points used here, strictly typed."""
    if not _windows_only():  # pragma: no cover - guarded by callers
        raise PrivateRootError("Windows security APIs are unavailable here")
    advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

    advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_ulong),
    ]
    advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = ctypes.c_int

    advapi32.GetNamedSecurityInfoW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.c_int,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(ctypes.c_void_p),
    ]
    advapi32.GetNamedSecurityInfoW.restype = ctypes.c_ulong

    advapi32.GetAclInformation.argtypes = [
        ctypes.c_void_p,
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.c_int,
    ]
    advapi32.GetAclInformation.restype = ctypes.c_int

    advapi32.GetAce.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    advapi32.GetAce.restype = ctypes.c_int

    advapi32.ConvertSidToStringSidW.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_wchar_p),
    ]
    advapi32.ConvertSidToStringSidW.restype = ctypes.c_int

    advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_wchar_p),
        ctypes.POINTER(ctypes.c_ulong),
    ]
    advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW.restype = ctypes.c_int

    kernel32.CreateDirectoryW.argtypes = [
        ctypes.c_wchar_p,
        ctypes.POINTER(_SecurityAttributes),
    ]
    kernel32.CreateDirectoryW.restype = ctypes.c_int

    kernel32.GetCurrentProcess.restype = ctypes.c_void_p
    kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel32.CloseHandle.restype = ctypes.c_int
    kernel32.LocalFree.argtypes = [ctypes.c_void_p]
    kernel32.LocalFree.restype = ctypes.c_void_p

    advapi32.OpenProcessToken.argtypes = [
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_void_p),
    ]
    advapi32.OpenProcessToken.restype = ctypes.c_int

    advapi32.GetTokenInformation.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_void_p,
        ctypes.c_ulong,
        ctypes.POINTER(ctypes.c_ulong),
    ]
    advapi32.GetTokenInformation.restype = ctypes.c_int

    return advapi32, kernel32


class _AclSizeInformation(ctypes.Structure):
    _fields_ = [
        ("AceCount", ctypes.c_ulong),
        ("AclBytesInUse", ctypes.c_ulong),
        ("AclBytesFree", ctypes.c_ulong),
    ]


_TOKEN_QUERY = 0x0008
_TOKEN_USER = 1
_OWNER_SECURITY_INFORMATION = 0x00000001


def _current_user_sid(api) -> str:
    advapi32 = api[0]
    kernel32 = api[1]
    token = ctypes.c_void_p()
    if not advapi32.OpenProcessToken(
        kernel32.GetCurrentProcess(), _TOKEN_QUERY, ctypes.byref(token)
    ):
        raise PrivateRootError(
            f"OpenProcessToken failed (error {ctypes.get_last_error()}); the "
            "fixture root's owner cannot be determined, so it is refused"
        )
    try:
        needed = ctypes.c_ulong(0)
        advapi32.GetTokenInformation(token, _TOKEN_USER, None, 0, ctypes.byref(needed))
        if needed.value == 0:
            raise PrivateRootError("GetTokenInformation returned no size")
        buffer = ctypes.create_string_buffer(needed.value)
        if not advapi32.GetTokenInformation(
            token, _TOKEN_USER, buffer, needed.value, ctypes.byref(needed)
        ):
            raise PrivateRootError(
                f"GetTokenInformation(TokenUser) failed (error {ctypes.get_last_error()})"
            )
        # TOKEN_USER is a single SID_AND_ATTRIBUTES: SID* first, attributes after.
        sid_pointer = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_void_p)).contents
        return _sid_to_string(api, sid_pointer)
    finally:
        kernel32.CloseHandle(token)


def _sid_to_string(api, sid_pointer) -> str:
    advapi32, kernel32 = api
    out = ctypes.c_wchar_p()
    if not advapi32.ConvertSidToStringSidW(sid_pointer, ctypes.byref(out)):
        raise PrivateRootError(
            f"ConvertSidToStringSid failed (error {ctypes.get_last_error()})"
        )
    try:
        return out.value
    finally:
        kernel32.LocalFree(ctypes.cast(out, ctypes.c_void_p))


def _owner_only_sddl(owner_sid: str) -> str:
    return (
        "D:PAI"
        f"(A;OICI;FA;;;SY)"
        f"(A;OICI;FA;;;BA)"
        f"(A;OICI;FA;;;{owner_sid})"
    )


def _windows_create(path: Path, api) -> None:
    advapi32, kernel32 = api
    user_sid = _current_user_sid(api)
    sddl = _owner_only_sddl(user_sid)
    descriptor = ctypes.c_void_p()
    if not advapi32.ConvertStringSecurityDescriptorToSecurityDescriptorW(
        sddl, _SDDL_REVISION_1, ctypes.byref(descriptor), None
    ):
        raise PrivateRootError(
            f"cannot build the owner-only security descriptor (error "
            f"{ctypes.get_last_error()})"
        )
    try:
        attributes = _SecurityAttributes()
        attributes.nLength = ctypes.sizeof(_SecurityAttributes)
        attributes.lpSecurityDescriptor = descriptor
        attributes.bInheritHandle = 0
        if not kernel32.CreateDirectoryW(str(path), ctypes.byref(attributes)):
            error = ctypes.get_last_error()
            raise PrivateRootError(
                f"CreateDirectoryW({path}) with an owner-only descriptor failed "
                f"(error {error})"
            )
    finally:
        kernel32.LocalFree(descriptor)


def _security_descriptor(api, path: Path):
    """Return `(descriptor_buffer, dacl_pointer, control, owner_sid_text)`."""
    advapi32, kernel32 = api
    owner = ctypes.c_void_p()
    group = ctypes.c_void_p()
    dacl = ctypes.c_void_p()
    sacl = ctypes.c_void_p()
    descriptor = ctypes.c_void_p()
    status = advapi32.GetNamedSecurityInfoW(
        str(path),
        _SE_FILE_OBJECT,
        _OWNER_SECURITY_INFORMATION | _DACL_SECURITY_INFORMATION,
        ctypes.byref(owner),
        ctypes.byref(group),
        ctypes.byref(dacl),
        ctypes.byref(sacl),
        ctypes.byref(descriptor),
    )
    if status != 0:
        raise PrivateRootError(
            f"GetNamedSecurityInfo({path}) failed with status {status}; the root's "
            "ACL cannot be read, so it is refused"
        )
    # The descriptor is self-relative; SECURITY_DESCRIPTOR_RELATIVE starts with
    # Revision(u8) Sbz1(u8) Control(u16), so the control word sits at offset 2.
    # (There is no ctypes binding for it and the value is all this needs.)
    control_pointer = ctypes.cast(
        ctypes.c_void_p(descriptor.value + 2), ctypes.POINTER(ctypes.c_ushort)
    )
    owner_sid = _sid_to_string(api, owner) if owner else None
    return descriptor, dacl, control_pointer.contents.value, owner_sid


_ACL_INFORMATION_CLASS = 2  # AclSizeInformation


def _aces(api, dacl) -> list[tuple[int, int, int, str]]:
    """`(ace_type, ace_flags, mask, trustee_sid)` for every ACE in the DACL."""
    advapi32, _kernel32 = api
    info = _AclSizeInformation()
    if not advapi32.GetAclInformation(
        dacl, ctypes.byref(info), ctypes.sizeof(info), _ACL_INFORMATION_CLASS
    ):
        raise PrivateRootError(
            f"GetAclInformation failed (error {ctypes.get_last_error()})"
        )
    entries: list[tuple[int, int, int, str]] = []
    for index in range(info.AceCount):
        ace_pointer = ctypes.c_void_p()
        if not advapi32.GetAce(dacl, index, ctypes.byref(ace_pointer)):
            raise PrivateRootError(
                f"GetAce({index}) failed (error {ctypes.get_last_error()})"
            )
        header = ctypes.cast(ace_pointer, ctypes.POINTER(_ACEHeader)).contents
        base = ace_pointer.value
        mask = ctypes.cast(
            ctypes.c_void_p(base + _MASK_OFFSET), ctypes.POINTER(ctypes.c_ulong)
        ).contents.value
        sid_offset = _SID_OFFSET_PLAIN
        if header.AceType in (
            _ACCESS_ALLOWED_OBJECT_ACE_TYPE,
            _ACCESS_DENIED_OBJECT_ACE_TYPE,
            _ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE,
            _ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE,
        ):
            flags = ctypes.cast(
                ctypes.c_void_p(base + _SID_OFFSET_PLAIN),
                ctypes.POINTER(ctypes.c_ulong),
            ).contents.value
            sid_offset = _SID_OFFSET_PLAIN + 4
            if flags & _ACE_OBJECT_TYPE_PRESENT:
                sid_offset += 16
            if flags & _ACE_INHERITED_OBJECT_TYPE_PRESENT:
                sid_offset += 16
        elif header.AceType not in (
            _ACCESS_ALLOWED_ACE_TYPE,
            _ACCESS_DENIED_ACE_TYPE,
            _ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
            _ACCESS_DENIED_CALLBACK_ACE_TYPE,
        ):
            raise PrivateRootError(
                f"the fixture root's DACL contains ACE type {header.AceType}, which "
                "this harness does not model; the root is refused"
            )
        sid = _sid_to_string(api, ctypes.c_void_p(base + sid_offset))
        entries.append((header.AceType, header.AceFlags, mask, sid))
    return entries


def _windows_assert(
    path: Path, api, require_protected_dacl: bool, require_inheritable_writers: bool = False
) -> None:
    """Refuse `path` unless its DACL grants write only to trusted trustees.

    `require_protected_dacl` is set for a *root*: the root must not inherit a
    single ACE, or its effective access would depend on a directory (the shared
    `%TEMP%`) that this harness does not control. Entries created *inside* a
    verified root are expected to carry inherited ACEs - they are inherited from
    that root's own protected DACL - so only their trustees are checked.
    """
    _advapi32, kernel32 = api
    descriptor, dacl, control, owner_sid = _security_descriptor(api, path)
    try:
        if not control & _SE_DACL_PRESENT:
            raise PrivateRootError(
                f"{path} has no DACL, so every user has full access; refused"
            )
        if not dacl:
            # A NULL DACL in a present-DACL descriptor grants everyone full access.
            raise PrivateRootError(
                f"{path} has a NULL DACL, which grants everyone full access; refused"
            )
        if require_protected_dacl and not control & _SE_DACL_PROTECTED:
            raise PrivateRootError(
                f"{path} inherits ACEs from its parent, so its effective access "
                "depends on a directory this harness does not control; refused"
            )
        user_sid = _current_user_sid(api)
        if owner_sid != user_sid:
            raise PrivateRootError(
                f"{path} is owned by {owner_sid}, not by the running user "
                f"({user_sid}); another user's root cannot be proven private, so "
                "it is refused"
            )
        # `OWNER RIGHTS` is the current user by the check above, so it stands in
        # for the owner's own SID.
        allowed = _TRUSTED_SIDS | {user_sid, _OWNER_RIGHTS_SID}
        owner_can_write = False
        for ace_type, _flags, mask, sid in _aces(api, dacl):
            if ace_type not in (_ACCESS_ALLOWED_ACE_TYPE, _ACCESS_DENIED_ACE_TYPE):
                raise PrivateRootError(
                    f"{path} grants access through an object or callback ACE "
                    f"(type {ace_type}); it cannot be proven private, so it is refused"
                )
            if ace_type == _ACCESS_DENIED_ACE_TYPE:
                if require_protected_dacl:
                    # Deny ACE ordering is not modelled here, and this harness
                    # only ever writes DACLs without one; a root that carries a
                    # deny ACE cannot be proven usable, so it is refused.
                    raise PrivateRootError(
                        f"{path} carries a deny ACE for {sid}; this check does not "
                        "model deny ordering, so the root cannot be proven usable"
                    )
                continue
            if (
                mask & _WRITE_RIGHTS
                and sid in {user_sid, _OWNER_RIGHTS_SID}
                and not _flags & _INHERIT_ONLY_ACE
                and mask & _CREATE_IN_DIRECTORY == _CREATE_IN_DIRECTORY
            ):
                # `INHERIT_ONLY` would apply to children but not to the root, and
                # a mask without add-file/add-subdirectory would not let the
                # harness create the fixture at all.
                owner_can_write = True
            if mask & _WRITE_RIGHTS and sid not in allowed:
                raise PrivateRootError(
                    f"{path} is writable by {sid} (mask {hex(mask)}), which is "
                    "outside the owner, SYSTEM and Administrators; a root another "
                    "OS user can write cannot be proven private, so it is refused"
                )
            if (
                require_inheritable_writers
                and mask & _WRITE_RIGHTS
                and _flags & _NO_PROPAGATE_INHERIT_ACE
            ):
                # `NO_PROPAGATE_INHERIT` stops the ACE after one generation, so a
                # grandchild would fall back to whatever the environment provides.
                raise PrivateRootError(
                    f"{path} grants write to {sid} with NO_PROPAGATE_INHERIT "
                    f"(flags {hex(_flags)}), so deeper entries would not carry the "
                    "restriction; refused"
                )
            if (
                require_inheritable_writers
                and mask & _WRITE_RIGHTS
                and _flags & _INHERIT_BOTH != _INHERIT_BOTH
            ):
                # A root's protection is only worth what its children inherit: an
                # owner-only ACE without OI|CI leaves newly created children
                # with an empty DACL (unusable) or whatever the parent directory
                # would have given them (not provable here).
                raise PrivateRootError(
                    f"{path} grants write to {sid} without object+container "
                    f"inheritance (flags {hex(_flags)}), so children of this root "
                    "would not be proven private; refused"
                )
        if require_protected_dacl and not owner_can_write:
            # An empty DACL denies everyone, including this harness: it is not a
            # private root, it is an unusable one, and declaring it private would
            # be a false positive (review round 7, Medium).
            raise PrivateRootError(
                f"{path} has no write ACE for its owner, so it is unusable rather "
                "than private; refused"
            )
    finally:
        kernel32.LocalFree(descriptor)


def trustee_report(path: Path) -> list[dict]:
    """Every ACE on `path` as evidence: trustee, mask, type and inheritance.

    Read-only. Used by the campaign evidence and by tests; it never modifies an
    ACL.
    """
    if not _windows_only():
        mode = stat.S_IMODE(path.lstat().st_mode)
        return [
            {
                "platform": "posix",
                "mode": oct(mode),
                "group_or_world_bits": bool(mode & 0o077),
            }
        ]
    api = _load_windows_api()
    _advapi32, kernel32 = api
    descriptor, dacl, control, owner_sid = _security_descriptor(api, path)
    try:
        rows: list[dict] = []
        if not dacl:
            return [
                {
                    "platform": "windows",
                    "dacl": "NULL",
                    "note": "every user has full access",
                }
            ]
        for ace_type, flags, mask, sid in _aces(api, dacl):
            rows.append(
                {
                    "platform": "windows",
                    "owner": owner_sid,
                    "ace_type": ace_type,
                    "inheritance_flags": flags,
                    "mask": hex(mask),
                    "write_rights": bool(mask & _WRITE_RIGHTS),
                    "trustee": sid,
                    "trusted_principal": sid in _TRUSTED_SIDS,
                    "dacl_protected": bool(control & _SE_DACL_PROTECTED),
                }
            )
        return rows
    finally:
        kernel32.LocalFree(descriptor)


# ---------------------------------------------------------------------------
# Public surface
# ---------------------------------------------------------------------------


def support_class() -> str:
    """Which mechanism this platform uses, for the evidence record."""
    return "windows-dacl" if _windows_only() else "posix-mode"


def create_owner_only_dir(path: Path) -> None:
    """Create `path` with an owner-only DACL/mode, atomically.

    Raises `PrivateRootError` rather than creating a root it cannot prove private;
    a refused creation leaves at most a directory without an ownership marker,
    which `make_fixture` will refuse to claim on a later run.
    """
    if _windows_only():
        api = _load_windows_api()
        _windows_create(path, api)
        _windows_assert(path, api, require_protected_dacl=True)
        return
    _posix_create(path)
    _posix_assert(path, expect_directory=True)


def assert_private_root(path: Path) -> None:
    """Refuse a fixture *root* unless it is provably owner-only.

    Stricter than `assert_private_entry`: a root may not inherit its DACL from an
    uncontrolled parent, and every write ACE on it must be inheritable, or the
    children this harness creates would not carry the same restriction.
    """
    if not path.is_dir():
        raise PrivateRootError(f"{path} is not a directory")
    if _windows_only():
        _windows_assert(
            path,
            _load_windows_api(),
            require_protected_dacl=True,
            require_inheritable_writers=True,
        )
        return
    _posix_assert(path, expect_directory=True)


def assert_private_entry(path: Path) -> None:
    """Refuse an entry inside a verified root unless it is owner-only.

    An inherited ACE is expected here (it comes from the root's own protected
    DACL); what is checked is that no *unexpected* trustee holds write access.
    """
    if not path.exists():
        raise PrivateRootError(f"{path} does not exist")
    if _windows_only():
        _windows_assert(
            path, _load_windows_api(), require_protected_dacl=False
        )
        return
    _posix_assert(path, expect_directory=path.is_dir())


def verify_private_tree(root: Path) -> list[str]:
    """Verify *every* entry below `root`, not a hand-picked list.

    A child's DACL can diverge from the root's (an explicit ACE, a replaced
    directory, a configuration where inheritance does not reach it), and the
    harness writes model-influenced files into `repo/`. Checking a fixed sample
    would leave the rest unproven, so the walk covers the whole tree and refuses
    on the first entry it cannot read.

    The walk never follows a link: an entry that is a reparse point or a symlink
    is refused (the driver's hazard scan refuses those separately, but this
    function must not be the weak link).
    """
    if not root.is_dir():
        raise PrivateRootError(f"{root} is not a directory")
    assert_private_root(root)
    stack = [root]
    checked: list[str] = []
    while stack:
        directory = stack.pop()
        try:
            entries = list(directory.iterdir())
        except OSError as error:
            raise PrivateRootError(
                f"{directory} cannot be read ({error}), so the tree cannot be "
                "proven private; refused"
            ) from error
        for entry in entries:
            try:
                info = entry.lstat()
            except OSError as error:
                raise PrivateRootError(
                    f"{entry} cannot be inspected ({error}); refused"
                ) from error
            if stat.S_ISLNK(info.st_mode):
                raise PrivateRootError(f"{entry} is a symbolic link; refused")
            if stat.S_ISDIR(info.st_mode):
                if _windows_only():
                    _windows_assert(
                        entry, _load_windows_api(), require_protected_dacl=False
                    )
                else:
                    _posix_assert(entry, expect_directory=True)
                stack.append(entry)
            elif stat.S_ISREG(info.st_mode):
                if _windows_only():
                    _windows_assert(
                        entry, _load_windows_api(), require_protected_dacl=False
                    )
                else:
                    _posix_assert(entry, expect_directory=False)
            else:
                raise PrivateRootError(
                    f"{entry} is neither a regular file nor a directory; refused"
                )
            checked.append(str(entry))
    return checked


def _windows_reentry_verdict(api, directory: Path) -> str | None:
    """Return a refusal reason when `directory` lets a stranger move a child."""
    if not directory.is_dir():
        return f"{directory} is not a directory"
    _advapi32, kernel32 = api
    descriptor, dacl, control, owner_sid = _security_descriptor(api, directory)
    try:
        if not dacl:
            return (
                f"{directory} has a NULL or absent DACL, so every user can rename "
                "or delete entries in it"
            )
        user_sid = _current_user_sid(api)
        if owner_sid != user_sid and owner_sid not in _TRUSTED_SIDS:
            # The owner of a directory can always change its DACL (the owner is
            # implicitly granted WRITE_DAC), so a foreign owner is a re-entry
            # route even when the current DACL looks harmless.
            return (
                f"{directory} is owned by {owner_sid}, which is neither the running "
                "user nor a trusted principal; its owner could grant itself the "
                "right to rename a fixture root there"
            )
        allowed = _TRUSTED_SIDS | {user_sid, _OWNER_RIGHTS_SID}
        owner_can_write = False
        for ace_type, _flags, mask, sid in _aces(api, dacl):
            if ace_type == _ACCESS_DENIED_ACE_TYPE:
                continue
            if _flags & _INHERIT_ONLY_ACE:
                # An inherit-only ACE does not apply to this directory itself, so it
                # cannot grant a rename or delete of *this* component. C:\ carries
                # exactly such an ACE for Authenticated Users, and the fixture root's
                # own DACL is protected, so nothing is inherited into it either.
                continue
            if ace_type != _ACCESS_ALLOWED_ACE_TYPE:
                # An object or callback ACE can grant the same right under a
                # condition this code does not evaluate; refuse rather than skip.
                return (
                    f"{directory} grants access through an object or callback ACE "
                    f"(type {ace_type}), which this check cannot evaluate"
                )
            if mask & _REENTRY_RIGHTS and sid not in allowed:
                return (
                    f"{directory} lets {sid} rename or delete entries in it (mask "
                    f"{hex(mask)}); a fixture root there can be replaced by a link"
                )
    finally:
        kernel32.LocalFree(descriptor)
    return None


def _posix_reentry_verdict(directory: Path) -> str | None:
    info = directory.lstat()
    mode = stat.S_IMODE(info.st_mode)
    if info.st_uid not in (os.geteuid(), 0):
        # The owner of a directory can always chmod it, so a foreign owner is a
        # re-entry route even when the mode looks harmless.
        return (
            f"{directory} is owned by uid {info.st_uid}, which is neither the "
            "running user nor root; its owner could grant itself write access and "
            "rename a fixture root there"
        )
    if mode & 0o022:
        if not info.st_mode & stat.S_ISVTX:
            return (
                f"{directory} is group- or world-writable (mode {oct(mode)}) and not "
                "sticky, so another user can rename a fixture root in it"
            )
        if info.st_uid == 0:
            # A sticky directory only lets the *entry* owner, the *directory*
            # owner and root remove an entry. Root and the entry owner are both
            # acceptable; a non-root foreign directory owner is refused above.
            return None
    return None


def assert_path_cannot_reenter(path: Path) -> None:
    """Refuse a path whose every component's *name* can be replaced by a stranger.

    A private root protects its *contents*; it does not protect its *name*. An
    attacker who may delete or rename a child of any directory on the path can
    move that component aside and put a symlink/junction in its place, and every
    later write the harness performs through the path lands wherever the link
    points. Round 6 covered the base; round 7 showed that the base's own
    *ancestors* are the same problem, so the whole chain up to the filesystem root
    is checked.

    * Windows: `FILE_DELETE_CHILD` on a directory deletes/renames a child
      regardless of the child's own DACL, so it is the bypass that matters.
      `WRITE_DAC` and `WRITE_OWNER` let a trustee grant itself that right first,
      and a directory's *owner* is implicitly allowed to rewrite the DACL, so a
      foreign owner is refused even when the current ACL looks harmless.
      `Modify` (0x1301bf) has none of those bits; `Full control` has all of them.
    * POSIX: a group/other-writable directory allows `rename(2)` of a child unless
      it is sticky; a sticky directory still lets its owner remove the entry, so a
      non-root foreign owner is refused. The directory owner can always `chmod`.
    """
    if not path.exists():
        raise PrivateRootError(f"{path} does not exist")
    chain = [path, *path.parents]
    if _windows_only():
        api = _load_windows_api()
        for directory in chain:
            reason = _windows_reentry_verdict(api, directory)
            if reason:
                raise PrivateRootError(f"{reason}; refused")
        return
    for directory in chain:
        reason = _posix_reentry_verdict(directory)
        if reason:
            raise PrivateRootError(f"{reason}; refused")


class RunLease:
    """An exclusive advisory lock on a fixture root, held for the whole run.

    Two runs that reuse one run id would otherwise share `repo/` and `state/`,
    and the second would rewrite the seeded files while the first is asking a
    model about them - which contaminate the first run's evidence even though the
    attempt directories themselves are separate. The lock is released by
    `release()` or by process exit.
    """

    def __init__(self, path: Path, handle: int):
        self.path = path
        self._handle: int | None = handle

    def release(self) -> None:
        if self._handle is None:
            return
        try:
            if _windows_only():
                import msvcrt

                msvcrt.locking(self._handle, msvcrt.LK_UNLCK, 1)
            else:
                import fcntl

                fcntl.flock(self._handle, fcntl.LOCK_UN)
        except OSError:
            # Closing the handle below releases the lock in every case that
            # matters; an explicit unlock failure is not worth failing a run.
            pass
        finally:
            os.close(self._handle)
            self._handle = None


def acquire_run_lease(root: Path) -> RunLease:
    """Take the exclusive lease on `root`, or refuse because another run has it."""
    path = root / "run-lease"
    handle = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        if _windows_only():
            import msvcrt

            msvcrt.locking(handle, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as error:
        os.close(handle)
        raise PrivateRootError(
            f"{root} is already in use by another run ({error}); two runs must not "
            "share one fixture root"
        ) from error
    return RunLease(path, handle)


def create_private_dir(path: Path) -> None:
    """Create a directory inside a verified root, owner-only on POSIX too.

    `mkdir` is masked by the process umask, so on POSIX the mode is set after
    creation. The directory belongs to this harness and sits inside its own root,
    so tightening it is not a decision about another directory.
    """
    if not path.is_dir():
        os.mkdir(path)
    if _windows_only():
        _windows_assert(path, _load_windows_api(), require_protected_dacl=False)
        return
    # An existing directory is one this harness created on an earlier attempt of
    # the same owned root, so tightening it is still its own decision.
    os.chmod(path, POSIX_OWNER_ONLY_MODE)
    _posix_assert(path, expect_directory=True)


def create_private_dir_exclusive(path: Path) -> None:
    """Create a directory that must not exist yet, owner-only.

    Raises `FileExistsError` when something is already there, so a caller that
    allocates numbered directories can move to the next number instead of sharing.
    """
    os.mkdir(path)
    if _windows_only():
        _windows_assert(path, _load_windows_api(), require_protected_dacl=False)
        return
    os.chmod(path, POSIX_OWNER_ONLY_MODE)
    _posix_assert(path, expect_directory=True)


def restrict_file(path: Path) -> None:
    """Make a file this harness just created owner-only.

    On Windows the file inherits the root's protected DACL, so nothing is done.
    On POSIX `write_text` uses the umask (0600 under a `077` umask, 0644 under the
    usual `022`), so the mode is set explicitly - it is this harness's own file
    inside its own root, not another directory's permission.
    """
    if _windows_only():
        return
    os.chmod(path, 0o600)


if __name__ == "__main__":  # pragma: no cover - operator convenience
    import json

    target = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(os.getcwd())
    print(json.dumps({"path": str(target), "aces": trustee_report(target)}, indent=2))
