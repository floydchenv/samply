use std::ops::Deref;

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw;
use windows::Win32::System::Diagnostics::Etw::{TdhEnumerateProviders, PROVIDER_ENUMERATION_INFO};

use super::etw_types::*;
use super::traits::*;

#[derive(Debug)]
pub enum TdhNativeError {
    /// Represents an standard IO Error
    IoError(std::io::Error),
}

impl From<std::io::Error> for TdhNativeError {
    fn from(err: std::io::Error) -> Self {
        TdhNativeError::IoError(err)
    }
}

pub(crate) type TdhNativeResult<T> = Result<T, TdhNativeError>;

pub fn schema_from_tdh(event: &Etw::EVENT_RECORD) -> TdhNativeResult<TraceEventInfoRaw> {
    let mut buffer_size = 0;
    unsafe {
        if Etw::TdhGetEventInformation(event, None, None, &mut buffer_size)
            != ERROR_INSUFFICIENT_BUFFER.0
        {
            return Err(TdhNativeError::IoError(std::io::Error::last_os_error()));
        }

        let mut buffer = TraceEventInfoRaw::alloc(buffer_size);
        if Etw::TdhGetEventInformation(
            event,
            None,
            Some(buffer.info_as_ptr() as *mut _),
            &mut buffer_size,
        ) != 0
        {
            return Err(TdhNativeError::IoError(std::io::Error::last_os_error()));
        }

        Ok(buffer)
    }
}

pub(crate) fn property_size(event: &EventRecord, name: &str) -> TdhNativeResult<u32> {
    let mut property_size = 0;

    let utf16_name = name.to_utf16();
    let desc = Etw::PROPERTY_DATA_DESCRIPTOR {
        ArrayIndex: u32::MAX,
        PropertyName: utf16_name.as_ptr() as u64,
        ..Default::default()
    };

    unsafe {
        let status = Etw::TdhGetPropertySize(event.deref(), None, &[desc], &mut property_size);
        if status != 0 {
            return Err(TdhNativeError::IoError(std::io::Error::from_raw_os_error(
                status as i32,
            )));
        }
    }

    Ok(property_size)
}

pub fn list_etw_providers() {
    let mut buffer_size: u32 = 0;
    let mut status: u32;

    // Query required buffer size
    unsafe {
        status = TdhEnumerateProviders(None, &mut buffer_size);
    }
    if status == ERROR_INSUFFICIENT_BUFFER.0 {
        let mut provider_info = vec![0u8; buffer_size as usize];
        let mut buffer_size_copied = buffer_size;

        // Retrieve provider information
        unsafe {
            status = TdhEnumerateProviders(
                Some(provider_info.as_mut_ptr() as *mut PROVIDER_ENUMERATION_INFO),
                &mut buffer_size_copied,
            );
        }
        if status == ERROR_SUCCESS.0 {
            let provider_info =
                unsafe { &*(provider_info.as_ptr() as *const PROVIDER_ENUMERATION_INFO) };
            let provider_info_array = provider_info.TraceProviderInfoArray.as_ptr();

            for i in 0..provider_info.NumberOfProviders {
                // windows-rs defines TraceProviderInfoArray as a fixed size array of 1 so we need to use get_unchecked to get the other things
                let provider_name_offset =
                    unsafe { *provider_info_array.offset(i as isize) }.ProviderNameOffset as usize;
                let provider_name_ptr = provider_info as *const PROVIDER_ENUMERATION_INFO as usize
                    + provider_name_offset;
                // Find the length of the null-terminated string
                let mut len = 0;
                while unsafe { *(provider_name_ptr as *const u16).add(len) } != 0 {
                    len += 1;
                }
                let provider_name = unsafe {
                    String::from_utf16(std::slice::from_raw_parts(
                        provider_name_ptr as *const u16,
                        len,
                    ))
                    .unwrap_or_else(|_| "Error converting to string".to_string())
                };

                let provider_guid =
                    &unsafe { *provider_info_array.offset(i as isize) }.ProviderGuid;
                let schema_source = unsafe { *provider_info_array.offset(i as isize) }.SchemaSource;

                println!(
                    "  {:?} - {} - {}",
                    provider_guid,
                    provider_name,
                    if schema_source == 0 {
                        "XML manifest"
                    } else {
                        "MOF"
                    }
                );
            }
        } else {
            println!("TdhEnumerateProviders failed with error code {status:?}");
        }
    } else {
        println!("TdhEnumerateProviders failed with error code {status:?}");
    }
}
