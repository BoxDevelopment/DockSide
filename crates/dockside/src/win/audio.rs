//! Audio endpoints: the speakers, headphones and microphones Windows knows about.

use windows::core::{GUID, PWSTR};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };

pub struct Endpoint {
    pub id: String,
    pub name: String,
    /// The system default for its direction.
    pub default: bool,
}

/// The working endpoints of one direction: `eRender` (outputs) or `eCapture` (inputs).
pub fn endpoints(flow: EDataFlow) -> windows::core::Result<Vec<Endpoint>> {
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let default = devices.GetDefaultAudioEndpoint(flow, eConsole).and_then(|d| take(d.GetId()?)).ok();
        let all = devices.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..all.GetCount()? {
            let d = all.Item(i)?;
            let id = take(d.GetId()?)?;
            let store = d.OpenPropertyStore(STGM_READ)?;
            let value = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME)?;
            let name = take(PropVariantToStringAlloc(&value)?)?;
            out.push(Endpoint { default: default.as_deref() == Some(id.as_str()), id, name });
        }
        Ok(out)
    }
}

/// A COM-allocated string as a `String`, freeing it.
pub unsafe fn take(p: PWSTR) -> windows::core::Result<String> {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const _));
    Ok(s)
}
