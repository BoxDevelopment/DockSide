//! System controls Spotlight can drive: display brightness (WMI, built-in
//! panels), the Windows power mode / Low Power Mode, and light / dark mode.

use std::ffi::c_void;

use windows::core::{s, w, BSTR, GUID, PCWSTR};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD};
use windows::core::VARIANT;
use windows::Win32::System::Wmi::*;
use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};

// ---------------------------------------------------------------------------
// Power mode (the Settings → Power "Power mode" slider)
// ---------------------------------------------------------------------------

/// Overlay schemes behind the power mode slider.
const BEST_EFFICIENCY: GUID = GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a);
const BALANCED: GUID = GUID::from_u128(0);
const BEST_PERFORMANCE: GUID = GUID::from_u128(0xded574b5_45a0_4f42_8737_46345c09c238);

type GetOverlay = unsafe extern "system" fn(*mut GUID) -> u32;
type SetOverlay = unsafe extern "system" fn(*const GUID) -> u32;

fn powrprof(name: windows::core::PCSTR) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe {
        let lib = LoadLibraryW(w!("powrprof.dll")).ok()?;
        GetProcAddress(lib, name)
    }
}

/// True while Windows runs in Best power efficiency or Energy Saver is on.
pub fn low_power_mode() -> bool {
    let mut status = SYSTEM_POWER_STATUS::default();
    let saver = unsafe { GetSystemPowerStatus(&mut status).is_ok() } && status.SystemStatusFlag == 1;
    let overlay = powrprof(s!("PowerGetEffectiveOverlayScheme")).and_then(|f| {
        let f: GetOverlay = unsafe { std::mem::transmute(f) };
        let mut g = GUID::zeroed();
        (unsafe { f(&mut g) } == 0).then_some(g)
    });
    saver || overlay == Some(BEST_EFFICIENCY)
}

pub fn set_power_mode(mode: crate::spotlight::commands::PowerMode) {
    use crate::spotlight::commands::PowerMode;
    let guid = match mode {
        PowerMode::Efficiency => BEST_EFFICIENCY,
        PowerMode::Balanced => BALANCED,
        PowerMode::Performance => BEST_PERFORMANCE,
    };
    match powrprof(s!("PowerSetActiveOverlayScheme")) {
        Some(f) => {
            let f: SetOverlay = unsafe { std::mem::transmute(f) };
            let r = unsafe { f(&guid) };
            if r != 0 {
                log!("power mode change failed: {r}");
            }
        }
        None => log!("power mode API unavailable"),
    }
}

// ---------------------------------------------------------------------------
// Light / dark mode
// ---------------------------------------------------------------------------

pub fn set_dark_mode(dark: bool) {
    let light: u32 = (!dark).into();
    unsafe {
        for name in [w!("AppsUseLightTheme"), w!("SystemUsesLightTheme")] {
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                name,
                REG_DWORD.0,
                Some(&light as *const _ as *const c_void),
                4,
            );
        }
    }
    // Apps re-read the theme on this broadcast. Sent from another thread: DockSide's
    // own windows are on this one, which is busy running the command.
    std::thread::spawn(|| unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("ImmersiveColorSet").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            1000,
            None,
        );
    });
}

// ---------------------------------------------------------------------------
// Brightness (WMI; laptops and other built-in panels)
// ---------------------------------------------------------------------------

/// Sets (`Some(level)`) or steps (`delta`) the built-in display's brightness,
/// off the UI thread since WMI can take a moment.
pub fn brightness(level: Option<u8>, delta: i8) {
    let spawned = std::thread::Builder::new().name("set-brightness".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = set_brightness(level, delta) {
            log!("brightness change failed: {e}");
        }
    });
    if let Err(e) = spawned {
        log!("brightness thread failed: {e}");
    }
}

unsafe fn set_brightness(level: Option<u8>, delta: i8) -> windows::core::Result<()> {
    let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
    let services = locator.ConnectServer(&BSTR::from("ROOT\\WMI"), &BSTR::new(), &BSTR::new(), &BSTR::new(), 0, &BSTR::new(), None)?;
    CoSetProxyBlanket(&services, 10, 0, PCWSTR::null(), RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?;

    let target = match level {
        Some(l) => l.min(100),
        None => {
            let current = first(&services, "SELECT CurrentBrightness FROM WmiMonitorBrightness")?
                .and_then(|o| {
                    let mut v = VARIANT::default();
                    o.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
                    u32::try_from(&v).ok().or_else(|| i32::try_from(&v).ok().map(|x| x as u32))
                })
                .unwrap_or(50);
            (current as i32 + delta as i32).clamp(0, 100) as u8
        }
    };

    let Some(instance) = first(&services, "SELECT * FROM WmiMonitorBrightnessMethods")? else {
        log!("no built-in display supports brightness control");
        return Ok(());
    };
    let mut path = VARIANT::default();
    instance.Get(w!("__PATH"), 0, &mut path, None, None)?;
    let path = BSTR::try_from(&path)?;

    let mut class = None;
    services.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None)?;
    let mut in_def = None;
    class.unwrap().GetMethod(w!("WmiSetBrightness"), 0, &mut in_def, std::ptr::null_mut())?;
    let params = in_def.unwrap().SpawnInstance(0)?;
    // WMI takes uint32 as VT_I4 (uint8 stays VT_UI1).
    params.Put(w!("Timeout"), 0, &VARIANT::from(1i32), 0)?;
    params.Put(w!("Brightness"), 0, &VARIANT::from(target), 0)?;
    services.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &params, None, None)?;
    Ok(())
}

unsafe fn first(services: &IWbemServices, query: &str) -> windows::core::Result<Option<IWbemClassObject>> {
    let rows = services.ExecQuery(
        &BSTR::from("WQL"),
        &BSTR::from(query),
        WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
        None,
    )?;
    let mut objs = [None];
    let mut n = 0u32;
    let _ = rows.Next(WBEM_INFINITE, &mut objs, &mut n);
    Ok(if n == 0 { None } else { objs[0].take() })
}
