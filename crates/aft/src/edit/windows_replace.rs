//! Windows replacement policy. Prepare metadata and named streams before rename,
//! which publishes the new file without removing the destination name first.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
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
    FindStreamInfoStandard, GetFileInformationByHandle, MoveFileExW, SetFileInformationByHandle,
    SetFileTime, BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY, FILE_RENAME_INFO, FILE_WRITE_ATTRIBUTES,
    MOVEFILE_REPLACE_EXISTING, READ_CONTROL, WIN32_FIND_DATAW, WIN32_FIND_STREAM_DATA,
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

fn named_stream_path(path: &Path, name: &[u16]) -> io::Result<std::path::PathBuf> {
    let suffix: Vec<_> = ":$DATA".encode_utf16().collect();
    if name.first() != Some(&u16::from(b':'))
        || !name.ends_with(&suffix)
        || name.len() <= suffix.len() + 1
    {
        return Err(io::Error::other("unsupported named stream description"));
    }
    let mut stream = path.as_os_str().to_os_string();
    stream.push(std::ffi::OsString::from_wide(name));
    Ok(stream.into())
}

fn copy_named_streams(
    destination: &Path,
    replacement: &Path,
    temp: &File,
    _fault: Fault,
) -> io::Result<()> {
    let old_streams = streams(destination)?;
    #[cfg(test)]
    let old_streams = match _fault {
        Fault::InvalidStreamName => {
            let mut names = old_streams;
            names.push(vec![b'!' as u16]);
            names
        }
        Fault::StreamsUnknown => return Err(io::Error::other("streams are unknown")),
        _ => old_streams,
    };
    let unnamed: Vec<_> = "::$DATA".encode_utf16().collect();
    let permissions = temp.metadata()?.permissions();
    let mut copied = false;
    for name in &old_streams {
        if *name == unnamed {
            continue;
        }
        // The temporary already has the source's protected discretionary
        // access-control list. Keep names in UTF-16 to avoid selecting a
        // different stream when a name is not valid Unicode.
        let copy = (|| {
            let mut source = File::open(named_stream_path(destination, name)?)?;
            let mut target = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(named_stream_path(replacement, name)?)?;
            io::copy(&mut source, &mut target)?;
            target.sync_all()
        })();
        copy.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "copying named stream {}: {error}",
                    String::from_utf16_lossy(name)
                ),
            )
        })?;
        copied = true;
    }
    if copied {
        // Writing a named stream can set ARCHIVE. Restore the prepared attributes so
        // stream copying does not change the existing metadata eligibility rule.
        temp.set_permissions(permissions)?;
    }
    let new_streams = streams(replacement)?;
    if !old_streams.iter().all(|name| new_streams.contains(name)) {
        return Err(io::Error::other("replacement is missing a copied stream"));
    }
    Ok(())
}

/// ReplaceFileW carries creation time, short name, object ID, compression and
/// encryption from the destination. Copy creation time and retain unusual
/// metadata by writing in place instead of silently discarding it. Ordinary DOS
/// attributes must already match: ReplaceFileW merges attributes, while rename
/// keeps only the replacement's attributes. Access/write times come from the
/// replacement in both paths. Windows regenerates short aliases during rename;
/// an existing short name is not a reason to give up atomic replacement.
fn prepare_metadata(destination: &Path, source: &File, replacement: &File) -> io::Result<()> {
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
        return Err(io::Error::other("metadata cannot be preserved by rename"));
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
    replacement.sync_all().map_err(|error| {
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
    InvalidStreamName,
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
    MoveFile,
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
    if method != Method::PosixRename {
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
    let dacl_equal = super::windows_dacl::matches(destination, replacement)?;
    #[cfg(test)]
    let dacl_equal = dacl_equal && !matches!(_fault, Fault::DaclMismatch);
    if !dacl_equal {
        return Err(io::Error::other("DACLs differ"));
    }
    copy_named_streams(destination, replacement, temp, _fault)?;
    #[cfg(test)]
    if matches!(_fault, Fault::MetadataUnknown) {
        return Err(io::Error::other("metadata is unknown"));
    }
    rename_name(destination)?;
    prepare_metadata(destination, source, temp)
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
    // in-place path instead of changing their meaning.
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
            // destination. Keep the original spelling and write in place.
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
    // limit (including NUL). Keep longer names on the in-place path.
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

pub(super) fn replace(destination: &Path, replacement: &Path) -> io::Result<bool> {
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
    let temp = match prepared {
        Ok(temp) => temp,
        Err(reason) => {
            record(Method::InPlace, &reason.to_string());
            return Ok(false);
        }
    };
    let reason = match posix_rename(&temp, destination, fault) {
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
            format!("rename unsupported: {error}")
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
            // a MoveFileExW replacement, then the caller's in-place rewrite
            // with a recovery copy. Other errors (such as missing paths or
            // disk/I/O failures) still abort without another method.
            format!("rename failed: {error}")
        }
        Err(error) => {
            record(Method::PosixRename, &format!("rename failed: {error}"));
            return Err(error);
        }
    };
    drop(temp);
    let replaced = move_file(destination, replacement)?;
    record(
        if replaced {
            Method::MoveFile
        } else {
            Method::InPlace
        },
        &reason,
    );
    Ok(replaced)
}

fn move_file(destination: &Path, replacement: &Path) -> io::Result<bool> {
    let (destination_w, replacement_w) = (wide(destination), wide(replacement));
    // Unlike ReplaceFileW with a backup, a same-volume replacement rename does
    // not move the destination away first. Do not enable cross-volume copy/delete.
    // MoveFileExW can refuse even delete-sharing readers, so give short-lived
    // reader handles a bounded opportunity to close before writing in place.
    for attempt in 0..64 {
        // SAFETY: both paths are NUL-terminated and outlive the call.
        if unsafe {
            MoveFileExW(
                replacement_w.as_ptr(),
                destination_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING,
            )
        } != 0
        {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER | ERROR_INVALID_FUNCTION)
        ) {
            return Ok(false);
        }
        if !matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(
                ERROR_ACCESS_DENIED
                    | ERROR_SHARING_VIOLATION
                    | ERROR_LOCK_VIOLATION
                    | ERROR_USER_MAPPED_FILE
            )
        ) {
            return Err(error);
        }
        if attempt < 63 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    Ok(false)
}

#[cfg(test)]
#[path = "windows_posix_tests.rs"]
mod tests;
