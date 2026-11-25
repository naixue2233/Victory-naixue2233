//! The D3D12 Agility SDK: which D3D12 runtime the host uses.
//!
//! Windows 10's in-box runtime stops at shader model 6.5 and refuses root
//! signatures that index the descriptor heaps directly (flags 0x400, 0x800),
//! which every translated pipeline uses, so there the app-local runtime is
//! what makes the title run at all. Windows 11's in-box runtime is new enough
//! on its own: the extra 1.620.2 copy only stands between the title and the
//! runtime Microsoft shipped for the system.
//!
//! D3D12 looks for two exports on the process's executable: a version and a
//! path. When the path names a folder with `D3D12Core.dll` in it, and the
//! system's own runtime is older than the version, that folder's runtime is
//! used; without the folder, the in-box one is. So the choice is made at
//! startup, before the first device: on Windows 11 the path is moved to a
//! folder that is not there, and the system's runtime is used
//! ([`choose`]). `VICTORY_D3D12_SDK=inbox` or `=agility` overrides the
//! system's own answer, for A/B runs (on Windows 10 `inbox` is the
//! `+0xb5a5f8` crash, which is what the switch exists to avoid).
//!
//! `build.rs` exports both symbols, and `cargo xtask agility` puts the
//! runtime in `target\<profile>\D3D12` beside the executable.
#![allow(non_upper_case_globals)]

/// The runtime's SDK version: the minor number of the
/// `Microsoft.Direct3D.D3D12` package `cargo xtask agility` fetches
/// (1.620.2). Keep the two in step.
#[unsafe(no_mangle)]
pub static mut D3D12SDKVersion: u32 = 620;

/// A C string pointer D3D12 reads once at device creation.
#[repr(transparent)]
pub struct SdkPath(*const std::ffi::c_char);

// SAFETY: points at a static, immutable C string, and is written once at
// startup before any device is made (see [`choose`]).
unsafe impl Sync for SdkPath {}

/// Where `D3D12Core.dll` is, relative to the executable.
#[unsafe(no_mangle)]
pub static mut D3D12SDKPath: SdkPath = SdkPath(c".\\D3D12\\".as_ptr());

/// A folder that is not there: D3D12 falls back to the system's runtime.
const NO_RUNTIME: &std::ffi::CStr = c".\\no-d3d12-runtime\\";

/// The folder `D3D12SDKPath` names, beside the running executable.
pub fn runtime_dir() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("D3D12"))
}

/// Which runtime this process will use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    /// The runtime beside the executable (the Agility SDK).
    Agility,
    /// The one that came with Windows.
    InBox,
}

impl Choice {
    pub fn name(self) -> &'static str {
        match self {
            Choice::Agility => "agility",
            Choice::InBox => "in-box",
        }
    }
}

/// What the system itself asks for: the Agility SDK before Windows 11, the
/// in-box runtime on it. `VICTORY_D3D12_SDK=inbox|agility` says otherwise.
fn wanted() -> Choice {
    match std::env::var("VICTORY_D3D12_SDK").as_deref() {
        Ok("inbox") => return Choice::InBox,
        Ok("agility") => return Choice::Agility,
        Ok(other) if !other.is_empty() => {
            tracing::warn!("VICTORY_D3D12_SDK={other}: expected inbox or agility, taking the system's own answer")
        }
        _ => {}
    }
    if windows_11() {
        Choice::InBox
    } else {
        Choice::Agility
    }
}

/// The system's build number, or 0 if it cannot be read.
fn build() -> u32 {
    // RTL_OSVERSIONINFOW.
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    // SAFETY: only the fields above are read by RtlGetVersion, and `size`
    // names this struct's size, which is what it asks for.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo {
        size: std::mem::size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: `info` is a live, whole RTL_OSVERSIONINFOW.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status == 0 {
        info.build
    } else {
        0
    }
}

/// Windows 11's first build number.
const WINDOWS_11: u32 = 22_000;

fn windows_11() -> bool {
    build() >= WINDOWS_11
}

/// Points the process at the runtime the system asks for, and says which.
/// Call before the first D3D12 device; later calls change nothing D3D12 has
/// already read.
pub fn choose() -> Choice {
    let choice = wanted();
    // A Windows 10 that has no runtime beside it is the crash this switch
    // exists for: say so, then let D3D12 have the in-box one anyway.
    let beside = runtime_dir().map(|d| d.join("D3D12Core.dll"));
    let there = beside.as_ref().is_some_and(|p| p.exists());
    if !there && choice == Choice::Agility {
        tracing::warn!(
            "no D3D12Core.dll beside the executable: Windows 10 needs one (`cargo xtask agility`); \
             the in-box runtime will refuse every root signature"
        );
    }
    if choice == Choice::InBox {
        // SAFETY: this runs before any device is made, on the thread `main`
        // starts on; D3D12 reads the exports once, at the first device. A
        // version of 0 is what tells D3D12 not to look for the SDK at all,
        // and the path points at a folder that is not there as well, so
        // neither answer can pull the app-local runtime in.
        unsafe {
            D3D12SDKVersion = 0;
            D3D12SDKPath = SdkPath(NO_RUNTIME.as_ptr());
        }
    }
    tracing::info!(
        "d3d12 runtime: {} (windows build {}, beside the executable: {})",
        choice.name(),
        build(),
        there
    );
    choice
}

/// The version D3D12 will read: 620 for the Agility SDK, 0 once the process
/// uses the in-box runtime (the export stays, the folder does not exist).
pub fn sdk_version() -> u32 {
    // SAFETY: a plain read of a value written once, at startup.
    unsafe { D3D12SDKVersion }
}
