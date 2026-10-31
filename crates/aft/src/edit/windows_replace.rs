//! Windows replacement policy. POSIX rename avoids ReplaceFileW's transient
//! name removal, but does not merge the old file's security or stream metadata.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION,
    ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    ERROR_USER_MAPPED_FILE, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    GetLengthSid, ACL, ATTRIBUTE_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileRenameInfoEx, FindClose, FindFirstFileW, FindFirstStreamW, FindNextStreamW,
    FindStreamInfoStandard, GetFileInformationByHandle, ReplaceFileW, SetFileInformationByHandle,
    SetFileTime, BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY, FILE_RENAME_INFO, FILE_WRITE_ATTRIBUTES,
    READ_CONTROL, WIN32_FIND_DATAW, WIN32_FIND_STREAM_DATA,
};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_OBJECT_ID;
use windows_sys::Win32::System::IO::DeviceIoControl;

// FileRenameInfoEx flags from the Windows SDK; windows-sys exposes the info
// class and union, but not these two flag constants.
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: the zeroed structure is writable and the handle remains open.
    let mut info = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// Number of hard links, or `None` when it cannot be read.
pub(super) fn link_count(path: &Path) -> Option<u32> {
    information(&File::open(path).ok()?)
        .ok()
        .map(|info| info.nNumberOfLinks)
}

fn owner(file: &File) -> io::Result<Vec<u8>> {
    let mut sid = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: output pointers are writable; the returned SID lives in the
    // allocated descriptor until LocalFree below.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut sid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let result = if sid.is_null() {
        Err(io::Error::other("file owner SID is absent"))
    } else {
        // SAFETY: GetSecurityInfo returned this valid SID in the descriptor.
        unsafe {
            Ok(std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec())
        }
    };
    // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
    unsafe {
        LocalFree(descriptor);
    }
    result
}

fn resource_attributes(file: &File) -> io::Result<Option<Vec<u8>>> {
    let mut acl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: all output pointers are writable and the file stays open.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as _,
            SE_FILE_OBJECT,
            ATTRIBUTE_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut acl,
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let bytes = if acl.is_null() {
        None
    } else {
        // SAFETY: the ACL returned by GetSecurityInfo spans AclSize bytes and
        // remains allocated until the descriptor is freed below.
        Some(unsafe {
            std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize).to_vec()
        })
    };
    // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
    unsafe {
        LocalFree(descriptor);
    }
    Ok(bytes)
}

struct FindHandle(windows_sys::Win32::Foundation::HANDLE);
impl Drop for FindHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns a successful FindFirst* handle.
        unsafe {
            FindClose(self.0);
        }
    }
}

fn streams(path: &Path) -> io::Result<Vec<Vec<u16>>> {
    let path = wide(path);
    // SAFETY: zero is a valid initial value for this output structure.
    let mut data: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
    // SAFETY: the path is terminated and the output buffer is writable.
    let handle = unsafe {
        FindFirstStreamW(
            path.as_ptr(),
            FindStreamInfoStandard,
            (&mut data as *mut WIN32_FIND_STREAM_DATA).cast(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }
    let handle = FindHandle(handle);
    let mut names = Vec::new();
    loop {
        names.push(
            data.cStreamName
                .iter()
                .copied()
                .take_while(|&c| c != 0)
                .collect(),
        );
        // SAFETY: handle is open and data is a writable output buffer.
        if unsafe { FindNextStreamW(handle.0, (&mut data as *mut WIN32_FIND_STREAM_DATA).cast()) }
            == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                Ok(names)
            } else {
                Err(error)
            };
        }
    }
}

/// ReplaceFileW carries creation time, short name, object ID, compression and
/// encryption from the destination. Copy creation time and defer unusual
/// metadata to ReplaceFileW instead of silently discarding it. Ordinary DOS
/// attributes must already match: ReplaceFileW merges attributes, while rename
/// keeps only the replacement's attributes. Access/write times come from the
/// replacement in both paths.
fn prepare_metadata(
    destination: &Path,
    source: &File,
    replacement: &File,
    replacement_path: &Path,
) -> io::Result<()> {
    let old = information(source)?;
    let new = information(replacement)?;
    if resource_attributes(source)? != resource_attributes(replacement)? {
        return Err(io::Error::other("security resource attributes differ"));
    }
    let ordinary = FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_NORMAL
        | FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED
        | FILE_ATTRIBUTE_TEMPORARY;
    if (old.dwFileAttributes | new.dwFileAttributes) & !ordinary != 0 {
        return Err(io::Error::other("metadata requires ReplaceFileW"));
    }
    if old.dwFileAttributes != new.dwFileAttributes {
        return Err(io::Error::other("file attributes differ"));
    }
    // SAFETY: the terminated path and writable output structure outlive the call.
    let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let path = wide(destination);
    let handle = unsafe { FindFirstFileW(path.as_ptr(), &mut found) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let _handle = FindHandle(handle);
    if found.cAlternateFileName[0] != 0 {
        return Err(io::Error::other("short name requires ReplaceFileW"));
    }
    let mut object_id = [0u8; 64];
    let mut returned = 0;
    // SAFETY: the handle is open; FSCTL_GET_OBJECT_ID has no input and writes
    // at most the supplied FILE_OBJECTID_BUFFER-sized output.
    let has_id = unsafe {
        DeviceIoControl(
            source.as_raw_handle() as _,
            FSCTL_GET_OBJECT_ID,
            std::ptr::null(),
            0,
            object_id.as_mut_ptr().cast(),
            object_id.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if has_id != 0 || io::Error::last_os_error().raw_os_error() != Some(ERROR_FILE_NOT_FOUND as i32)
    {
        return Err(io::Error::other("object ID is present or unknown"));
    }
    // SAFETY: the replacement handle has FILE_WRITE_ATTRIBUTES. Null access
    // and write times keep the replacement's times, just as ReplaceFileW does.
    if unsafe {
        SetFileTime(
            replacement.as_raw_handle() as _,
            &old.ftCreationTime,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    crate::durability::sync_file(replacement, replacement_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("syncing replacement metadata: {error}"),
        )
    })
}

#[derive(Clone, Copy, Debug, Default)]
enum Fault {
    #[default]
    None,
    #[cfg(test)]
    OwnerMismatch,
    #[cfg(test)]
    OwnerUnknown,
    #[cfg(test)]
    ExtraStream,
    #[cfg(test)]
    StreamsUnknown,
    #[cfg(test)]
    DaclMismatch,
    #[cfg(test)]
    LinksUnknown,
    #[cfg(test)]
    MetadataUnknown,
    #[cfg(test)]
    Rename(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    PosixRename,
    ReplaceFile,
    InPlace,
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Fault> = const { std::cell::Cell::new(Fault::None) };
    static LAST_METHOD: std::cell::Cell<Option<Method>> = const { std::cell::Cell::new(None) };
    static LAST_REASON: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    static FALLBACK_COUNTS: std::cell::RefCell<std::collections::BTreeMap<String, usize>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

fn record(method: Method, reason: &str) {
    crate::slog_debug!("source_replace method={method:?} reason={reason}");
    #[cfg(test)]
    LAST_METHOD.set(Some(method));
    #[cfg(test)]
    LAST_REASON.with(|last| last.replace(reason.to_owned()));
    #[cfg(test)]
    if method == Method::ReplaceFile {
        FALLBACK_COUNTS
            .with(|counts| *counts.borrow_mut().entry(reason.to_owned()).or_default() += 1);
    }
}

fn eligible(
    destination: &Path,
    replacement: &Path,
    source: &File,
    temp: &File,
    _fault: Fault,
) -> io::Result<()> {
    let links = Some(information(source)?.nNumberOfLinks);
    #[cfg(test)]
    let links = if matches!(_fault, Fault::LinksUnknown) {
        None
    } else {
        links
    };
    if links != Some(1) {
        return Err(io::Error::other("link count is not one"));
    }
    let owners_equal = owner(source)? == owner(temp)?;
    #[cfg(test)]
    let owners_equal = match _fault {
        Fault::OwnerMismatch => false,
        Fault::OwnerUnknown => return Err(io::Error::other("owner is unknown")),
        _ => owners_equal,
    };
    if !owners_equal {
        return Err(io::Error::other("owner SIDs differ"));
    }
    let old_streams = streams(destination)?;
    let new_streams = streams(replacement)?;
    #[cfg(test)]
    let old_streams = match _fault {
        Fault::ExtraStream => {
            let mut names = old_streams;
            names.push(vec![b'!' as u16]);
            names
        }
        Fault::StreamsUnknown => return Err(io::Error::other("streams are unknown")),
        _ => old_streams,
    };
    if !old_streams.iter().all(|name| new_streams.contains(name)) {
        return Err(io::Error::other("destination has additional streams"));
    }
    let dacl_equal = super::windows_dacl::matches(destination, replacement)?;
    #[cfg(test)]
    let dacl_equal = dacl_equal && !matches!(_fault, Fault::DaclMismatch);
    if !dacl_equal {
        return Err(io::Error::other("DACLs differ"));
    }
    #[cfg(test)]
    if matches!(_fault, Fault::MetadataUnknown) {
        return Err(io::Error::other("metadata is unknown"));
    }
    rename_name(destination)?;
    prepare_metadata(destination, source, temp, replacement)
}

fn ordinary_component_preserves_name(component: &str) -> bool {
    if component.is_empty()
        || component.ends_with([' ', '.'])
        || component
            .chars()
            .any(|c| c <= '\u{1f}' || matches!(c, '<' | '>' | ':' | '"' | '/' | '|' | '?' | '*'))
    {
        return false;
    }
    // The device-name check ignores extensions and trims spaces from the stem.
    // Win32 also recognizes superscript 1/2/3 in COM and LPT device names.
    let stem = component.split('.').next().unwrap_or("").trim_matches(' ');
    let upper = stem.to_ascii_uppercase();
    !matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(*prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn rename_name(destination: &Path) -> io::Result<Vec<u16>> {
    // SetFileInformationByHandle accepts DOS/UNC names, not the verbatim
    // namespace returned by canonicalize. Keep unsupported namespaces on the
    // ReplaceFileW path instead of changing their meaning.
    use std::path::{Component, Prefix};
    let absolute = std::path::absolute(destination)?;
    let text = absolute
        .to_str()
        .ok_or_else(|| io::Error::other("rename namespace is unsupported"))?;
    if let Some(Component::Prefix(prefix)) = absolute.components().next() {
        let tail = match prefix.kind() {
            Prefix::VerbatimDisk(_) => text
                .strip_prefix(r"\\?\")
                .and_then(|drive| drive.get(2..).and_then(|tail| tail.strip_prefix('\\'))),
            Prefix::VerbatimUNC(..) => text.get(8..),
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                return Err(io::Error::other("rename namespace is unsupported"));
            }
            _ => None,
        };
        if matches!(
            prefix.kind(),
            Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..)
        ) && (text.contains('/')
            || !tail.is_some_and(|tail| tail.split('\\').all(ordinary_component_preserves_name)))
        {
            // Removing the prefix enables Win32 parsing and can select a different
            // destination. ReplaceFileW can use the original verbatim spelling.
            return Err(io::Error::other("destination requires verbatim spelling"));
        }
    }
    let path = crate::windows_path::normalize_windows_path(&absolute);
    if matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(
        prefix.kind(), Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(_)
    )) {
        return Err(io::Error::other("rename namespace is unsupported"));
    }
    let name: Vec<_> = path.as_os_str().encode_wide().collect();
    // Without the verbatim prefix, Win32 may impose the 260-code-unit path
    // limit (including NUL). Keep longer names on ReplaceFileW.
    if name.len() >= 260
        && matches!(absolute.components().next(), Some(Component::Prefix(prefix)) if matches!(
            prefix.kind(), Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..)
        ))
    {
        return Err(io::Error::other("destination requires verbatim spelling"));
    }
    Ok(name)
}

fn posix_rename(temp: &File, destination: &Path, _fault: Fault) -> io::Result<()> {
    #[cfg(test)]
    if let Fault::Rename(code) = _fault {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let mut name = rename_name(destination)?;
    let name_bytes =
        u32::try_from(name.len() * 2).map_err(|_| io::Error::other("rename path is too long"))?;
    // FileNameLength excludes the terminator, but the Win32 API's path
    // conversion also consumes a NUL-terminated string. Explicit termination
    // prevents allocator padding from becoming part of the destination name.
    name.push(0);
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = offset
        .checked_add(name.len() * 2)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| io::Error::other("rename path is too long"))?;
    let mut buffer = vec![
        0u64;
        (size as usize)
            .max(std::mem::size_of::<FILE_RENAME_INFO>())
            .div_ceil(8)
    ];
    // SAFETY: the u64 buffer is aligned for FILE_RENAME_INFO and sized for its
    // header and full counted UTF-16 name. The file handle remains open.
    let ok = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(
            temp.as_raw_handle() as _,
            FileRenameInfoEx,
            info.cast(),
            size,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn replace(destination: &Path, replacement: &Path, parent: &Path) -> io::Result<bool> {
    #[cfg(not(test))]
    let fault = Fault::None;
    #[cfg(test)]
    let fault = FAULT.get();
    let prepared = (|| {
        let source = File::open(destination)?;
        let temp = OpenOptions::new()
            .access_mode(DELETE | READ_CONTROL | FILE_WRITE_ATTRIBUTES | GENERIC_WRITE)
            .open(replacement)?;
        eligible(destination, replacement, &source, &temp, fault)?;
        Ok::<_, io::Error>(temp)
    })();
    match prepared {
        Ok(temp) => match posix_rename(&temp, destination, fault) {
            Ok(()) => {
                record(Method::PosixRename, "eligible");
                return Ok(true);
            }
            Err(error)
                if matches!(
                    error.raw_os_error().map(|code| code as u32),
                    Some(ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION)
                ) =>
            {
                // The volume or API does not support POSIX rename. The benchmark
                // counts these under "rename unsupported:" as unsupported-filesystem
                // fallbacks, separate from per-file refusals below.
                record(Method::ReplaceFile, &format!("rename unsupported: {error}"));
            }
            Err(error)
                if matches!(
                    error.raw_os_error().map(|code| code as u32),
                    Some(
                        ERROR_ACCESS_DENIED
                            | ERROR_SHARING_VIOLATION
                            | ERROR_LOCK_VIOLATION
                            | ERROR_USER_MAPPED_FILE
                    )
                ) =>
            {
                // Delete-access restrictions, sharing, locks and mappings can
                // prohibit replacement but permit writing the existing file. Try
                // ReplaceFileW, then the caller's in-place rewrite with a recovery
                // copy. Other errors (such as missing paths or disk/I/O failures)
                // still abort without another method.
                record(Method::ReplaceFile, &format!("rename failed: {error}"));
            }
            Err(error) => {
                record(Method::PosixRename, &format!("rename failed: {error}"));
                return Err(error);
            }
        },
        Err(reason) => record(Method::ReplaceFile, &reason.to_string()),
    }
    let replaced = replace_file(destination, replacement, parent)?;
    if !replaced {
        record(Method::InPlace, "ReplaceFileW failed");
    }
    Ok(replaced)
}

fn replace_file(destination: &Path, replacement: &Path, parent: &Path) -> io::Result<bool> {
    // With a backup name, every documented failure leaves the original
    // either under its own name or under the backup name, never gone.
    let backup = parent.join(format!(".aft-source-{}.bak", super::random_suffix()?));
    let (destination_w, replacement_w, backup_w) =
        (wide(destination), wide(replacement), wide(&backup));
    // SAFETY: all three paths are NUL-terminated and outlive the call;
    // the two reserved pointers must be null.
    let ok = unsafe {
        ReplaceFileW(
            destination_w.as_ptr(),
            replacement_w.as_ptr(),
            backup_w.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok != 0 {
        let _ = std::fs::remove_file(&backup);
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if !destination.exists() && backup.exists() {
        if let Err(restore) = std::fs::rename(&backup, destination) {
            return Err(io::Error::new(error.kind(), format!(
                "replacing {} failed ({error}) and moving its original back also failed ({restore}); the original bytes are in {}",
                destination.display(), backup.display())));
        }
    } else if destination.exists() {
        let _ = std::fs::remove_file(&backup);
    }
    Ok(false)
}

#[cfg(test)]
#[path = "windows_posix_tests.rs"]
mod tests;
