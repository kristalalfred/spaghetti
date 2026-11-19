use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{HSTRING, PWSTR};

/// Claude: a chain of parents longer than this is a loop of reused process IDs.
const MAX_DEPTH: usize = 64;

/// Claude: the processes running at one moment: each one's parent and executable name.
pub struct Snapshot {
    processes: HashMap<u32, (u32, String)>,
}

impl Snapshot {
    pub fn take() -> Self {
        let mut processes = HashMap::new();
        let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return Self { processes };
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
            processes.insert(entry.th32ProcessID, (entry.th32ParentProcessID, exe));
            more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        let _ = unsafe { CloseHandle(snapshot) };
        Self { processes }
    }

    pub fn contains(&self, pid: u32) -> bool {
        self.processes.contains_key(&pid)
    }

    fn parent(&self, pid: u32) -> Option<u32> {
        let &(parent, _) = self.processes.get(&pid)?;
        (parent != pid && self.processes.contains_key(&parent)).then_some(parent)
    }

    fn exe(&self, pid: u32) -> Option<&str> {
        self.processes.get(&pid).map(|(_, exe)| exe.as_str())
    }

    fn ancestors(&self, pid: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::successors(self.parent(pid), |&p| self.parent(p)).take(MAX_DEPTH)
    }

    /// Claude: the outermost process of the same executable that `pid` runs
    /// under, which the app's other processes run under too: a browser's
    /// audio process belongs to the browser.
    pub fn app_root(&self, pid: u32) -> u32 {
        let Some(exe) = self.exe(pid) else {
            return pid;
        };
        let mut root = pid;
        for parent in self.ancestors(pid) {
            if !self
                .exe(parent)
                .is_some_and(|e| e.eq_ignore_ascii_case(exe))
            {
                break;
            }
            root = parent;
        }
        root
    }

    /// Claude: the processes in `pids` that run under none of the others.
    pub fn roots(&self, pids: &[u32]) -> Vec<u32> {
        let mut roots: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|&pid| !self.ancestors(pid).any(|p| pids.contains(&p)))
            .collect();
        roots.sort_unstable();
        roots.dedup();
        roots
    }
}

struct Process(HANDLE);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub fn path(pid: u32) -> Option<String> {
    let process =
        Process(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut buffer = vec![0u16; 32 * 1024];
    let mut len = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buffer[..len as usize]))
}

/// Claude: the name an executable gives itself in its version resource, as
/// Task Manager shows it, or its file name without the extension.
pub fn display_name(path: &str) -> String {
    description(path).unwrap_or_else(|| {
        Path::new(path).file_stem().map_or_else(
            || path.to_string(),
            |stem| stem.to_string_lossy().into_owned(),
        )
    })
}

fn description(path: &str) -> Option<String> {
    let path = HSTRING::from(path);
    let size = unsafe { GetFileVersionInfoSizeW(&path, None) };
    if size == 0 {
        return None;
    }
    let mut info = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(&path, None, size, info.as_mut_ptr().cast()) }.ok()?;
    let translation = query(&info, "\\VarFileInfo\\Translation")?;
    let (language, codepage) = match translation {
        [l0, l1, c0, c1, ..] => (
            u16::from_le_bytes([*l0, *l1]),
            u16::from_le_bytes([*c0, *c1]),
        ),
        _ => return None,
    };
    let key = format!("\\StringFileInfo\\{language:04x}{codepage:04x}\\FileDescription");
    let value = query(&info, &key)?;
    let units: Vec<u16> = value
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&c| c != 0)
        .collect();
    Some(String::from_utf16_lossy(&units).trim().to_string()).filter(|name| !name.is_empty())
}

fn query<'a>(info: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let mut value: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let found = unsafe {
        VerQueryValueW(
            info.as_ptr().cast(),
            &HSTRING::from(key),
            &mut value,
            &mut len,
        )
    };
    if !found.as_bool() || value.is_null() {
        return None;
    }
    let start = value as usize - info.as_ptr() as usize;
    let bytes = if key.starts_with("\\StringFileInfo") {
        len as usize * 2
    } else {
        len as usize
    };
    info.get(start..start + bytes)
}
