//! The setup's store (TODO 19, the per-cable design, section 4): `HKLM\SOFTWARE\ACX-LL-Cable\Cables\<label>`, with `Id` (the
//! identity's 36 lower-case characters, REG_SZ), `N` (the instance number its node had last, REG_DWORD) and `Mics` (`both`,
//! `call` or `low-latency`, REG_SZ). `add` reuses a label's identity and n, `remove` keeps them, `remove --forget` drops them.
//! `Pending` (REG_DWORD 1, ours) marks a cable the migration is to make and hasn't yet: written before the old devices go, so
//! a run that stops after their removal is finished by the next (Install-Driver.ps1 runs the migration while one is pending);
//! cleared when the cable is made. `Replaces` (REG_MULTI_SZ, ours) goes with it: the instance IDs of the old driver's nodes the
//! migration makes the cable in place of, so a run after their removal still finds their endpoints for the endpoint map. The
//! key takes HKLM\SOFTWARE's permissions: any user reads it, administrators write it.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_DWORD, REG_MULTI_SZ, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
    REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use super::devices::{multi_sz_bytes, parse_multi_sz, wide_of_bytes};
use super::plan::Entry;
use crate::identity::{CableId, Label};
use crate::variant::Variant;
use crate::win_error::WinError;

/// The store's key under HKLM.
pub const KEY: &str = r"SOFTWARE\ACX-LL-Cable\Cables";
/// The value marking a cable the migration hasn't made yet.
pub const PENDING: &str = "Pending";
/// The old driver's nodes a pending cable is made in place of.
pub const REPLACES: &str = "Replaces";

/// What the store holds: the entries that read, and what was wrong with those that didn't (left alone, and reported).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
    pub entries: Vec<Entry>,
    pub problems: Vec<String>,
}

/// An entry from its key's name and values (pure).
pub fn entry_of(label: &str, id: Option<&str>, n: Option<u32>, mics: Option<&str>, pending: Option<u32>) -> Result<Entry, String> {
    let at = format!(r"HKLM\{KEY}\{label}");
    let label: Label = label.parse().map_err(|e| format!("{at}: {e}"))?;
    let id: CableId = id.ok_or_else(|| format!("{at}: no Id"))?.parse().map_err(|e| format!("{at}: {e}"))?;
    let n = n.filter(|&n| n >= 1).ok_or_else(|| format!("{at}: no N of 1 or more"))?;
    let mics = match mics {
        None => None,
        Some(m) => Some(Variant::from_arg(m).ok_or_else(|| format!("{at}: Mics '{m}' isn't both, call or low-latency"))?),
    };
    Ok(Entry { label, id, n, mics, pending: pending.is_some_and(|p| p != 0), replaces: Vec::new() })
}

/// A registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: our key, closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn check(call: &'static str, e: WIN32_ERROR) -> Result<(), WinError> {
    if e == ERROR_SUCCESS { Ok(()) } else { Err(WinError { call, code: e.0 }) }
}

fn open(path: &str, access: REG_SAM_FLAGS) -> Result<Option<Key>, WinError> {
    let path = HSTRING::from(path);
    let mut key = HKEY::default();
    // SAFETY: a predefined root; a NUL-terminated path; `key` is a live out-pointer, closed by Key.
    let e = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, &path, None, access | KEY_WOW64_64KEY, &mut key) };
    if e == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check("RegOpenKeyExW", e)?;
    Ok(Some(Key(key)))
}

/// A value's bytes and type, `None` if it isn't there.
fn value(key: &Key, name: &str) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, WinError> {
    let name = HSTRING::from(name);
    let mut buf = vec![0u8; 256];
    loop {
        let (mut kind, mut len) = (REG_VALUE_TYPE::default(), buf.len() as u32);
        // SAFETY: an open key; a NUL-terminated name; the buffer holds `len` bytes; live out-pointers.
        let e = unsafe { RegQueryValueExW(key.0, &name, None, Some(&mut kind), Some(buf.as_mut_ptr()), Some(&mut len)) };
        match e {
            ERROR_SUCCESS => {
                buf.truncate(len as usize);
                return Ok(Some((kind, buf)));
            }
            ERROR_FILE_NOT_FOUND => return Ok(None),
            ERROR_MORE_DATA => buf.resize(len as usize, 0),
            e => return Err(WinError { call: "RegQueryValueExW", code: e.0 }),
        }
    }
}

fn string_value(key: &Key, name: &str) -> Result<Option<String>, WinError> {
    Ok(value(key, name)?.filter(|(k, _)| *k == REG_SZ).map(|(_, b)| {
        let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|&p| u16::from_le_bytes(p)).collect();
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }))
}

fn dword_value(key: &Key, name: &str) -> Result<Option<u32>, WinError> {
    Ok(value(key, name)?.filter(|(k, b)| *k == REG_DWORD && b.len() == 4).map(|(_, b)| u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
}

/// A REG_MULTI_SZ's strings; none if it isn't there (or isn't one).
fn strings_value(key: &Key, name: &str) -> Result<Vec<String>, WinError> {
    Ok(value(key, name)?.filter(|(k, _)| *k == REG_MULTI_SZ).map(|(_, b)| parse_multi_sz(&wide_of_bytes(&b))).unwrap_or_default())
}

/// Deletes a value of an open key; not there is fine.
fn delete_value(key: &Key, name: &str, call: &'static str) -> Result<(), WinError> {
    let name = HSTRING::from(name);
    // SAFETY: an open key; no subkey; a NUL-terminated value name.
    match unsafe { RegDeleteKeyValueW(key.0, PCWSTR::null(), &name) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        e => Err(WinError { call, code: e.0 }),
    }
}

/// Every entry. Any user may read it; no store yet is an empty one.
pub fn read() -> Result<Store, WinError> {
    let mut store = Store::default();
    let Some(root) = open(KEY, KEY_READ)? else { return Ok(store) };
    for index in 0.. {
        // A key name is at most 255 characters
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: an open key; the buffer holds `len` characters; live out-pointers.
        let e = unsafe { RegEnumKeyExW(root.0, index, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) };
        if e == ERROR_NO_MORE_ITEMS {
            break;
        }
        check("RegEnumKeyExW", e)?;
        let label = String::from_utf16_lossy(&name[..len as usize]);
        let Some(key) = open(&format!(r"{KEY}\{label}"), KEY_READ)? else { continue };
        let id = string_value(&key, "Id")?;
        let n = dword_value(&key, "N")?;
        let mics = string_value(&key, "Mics")?;
        let pending = dword_value(&key, PENDING)?;
        match entry_of(&label, id.as_deref(), n, mics.as_deref(), pending) {
            Ok(mut e) => {
                if e.pending {
                    e.replaces = strings_value(&key, REPLACES)?;
                }
                store.entries.push(e)
            }
            Err(why) => store.problems.push(why),
        }
    }
    store.entries.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(store)
}

/// Records an entry (elevated).
pub fn write(e: &Entry) -> Result<(), WinError> {
    let path = HSTRING::from(format!(r"{KEY}\{}", e.label));
    let mut key = HKEY::default();
    // SAFETY: a predefined root; a NUL-terminated path; no class; `key` is a live out-pointer, closed by Key.
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &path,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE | KEY_WOW64_64KEY,
            None,
            &mut key,
            None,
        )
    };
    check("RegCreateKeyExW", r)?;
    let key = Key(key);
    let sz = |s: &str| s.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>();
    let set = |name: &str, kind: REG_VALUE_TYPE, data: &[u8]| {
        let name = HSTRING::from(name);
        // SAFETY: an open key; a NUL-terminated name; the data's length goes with it.
        check("RegSetValueExW", unsafe { RegSetValueExW(key.0, &name, None, kind, Some(data)) })
    };
    set("Id", REG_SZ, &sz(&e.id.to_string()))?;
    set("N", REG_DWORD, &e.n.to_le_bytes())?;
    if let Some(m) = e.mics {
        set("Mics", REG_SZ, &sz(m.arg()))?;
    }
    // Pending last when set, first when cleared: a Pending entry always has its Replaces
    if e.pending {
        if e.replaces.is_empty() {
            delete_value(&key, REPLACES, "RegDeleteKeyValueW(Replaces)")?;
        } else {
            let replaces: Vec<&str> = e.replaces.iter().map(String::as_str).collect();
            set(REPLACES, REG_MULTI_SZ, &multi_sz_bytes(&replaces))?;
        }
        set(PENDING, REG_DWORD, &1u32.to_le_bytes())
    } else {
        delete_value(&key, PENDING, "RegDeleteKeyValueW(Pending)")?;
        delete_value(&key, REPLACES, "RegDeleteKeyValueW(Replaces)")
    }
}

/// Drops a label's entry (`remove --forget`, elevated). Not there is fine.
pub fn forget(label: &Label) -> Result<(), WinError> {
    let Some(root) = open(KEY, KEY_READ | KEY_WRITE)? else { return Ok(()) };
    let sub = HSTRING::from(label.as_str());
    // SAFETY: an open key; a NUL-terminated subkey name.
    let e = unsafe { RegDeleteTreeW(root.0, &sub) };
    if e == ERROR_FILE_NOT_FOUND { Ok(()) } else { check("RegDeleteTreeW", e) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    #[test]
    fn an_entry_reads_from_its_values() {
        let e = entry_of("b", Some(ID), Some(3), Some("call"), None).unwrap();
        assert_eq!((e.label.as_str(), e.id.to_string(), e.n, e.mics, e.pending), ("B", ID.to_string(), 3, Some(Variant::Call), false));
        assert_eq!(entry_of("B", Some(&ID.to_uppercase()), Some(1), None, Some(0)).unwrap().mics, None);
        assert!(entry_of("B", Some(ID), Some(1), None, Some(1)).unwrap().pending, "the migration's, not made yet");
        for (label, id, n, mics, why) in [
            ("B_", Some(ID), Some(1), None, "isn't a cable label"),
            ("B", None, Some(1), None, "no Id"),
            ("B", Some("x"), Some(1), None, "isn't a cable id"),
            ("B", Some(ID), None, None, "no N"),
            ("B", Some(ID), Some(0), None, "no N"),
            ("B", Some(ID), Some(1), Some("two"), "Mics 'two'"),
        ] {
            let e = entry_of(label, id, n, mics, None).unwrap_err();
            assert!(e.contains(why) && e.starts_with(r"HKLM\SOFTWARE\ACX-LL-Cable\Cables\"), "{e}");
        }
    }

    /// Read-only: whatever the box's store holds reads without error (an absent store is empty).
    #[test]
    fn the_store_reads() {
        assert!(read().is_ok());
    }
}
