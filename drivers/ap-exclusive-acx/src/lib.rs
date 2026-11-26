// Pure-Rust `#![no_std]` KMDF 1.31 + ACX 1.1 Virtual Render Audio Driver
// Production opt-in exclusive-mode target ("Audiophile Player") for Tidal Desktop & WASAPI Exclusive.
// SPDX-License-Identifier: GPL-3.0-or-later
#![no_std]

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unsafe_op_in_unsafe_fn,
    unnecessary_transmutes,
    clippy::all
)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

mod acx_wdf;
mod ring;
mod stream;

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;

use ap_stream_protocol::{
    ApDriverConfig, ApDriverStatus, ApPcmChunkHeader, IOCTL_AP_GET_STATUS, IOCTL_AP_READ_PCM,
    IOCTL_AP_RESET_RING, IOCTL_AP_SET_CONFIG,
};

use acx_wdf::*;
use sys::*;

/// Required by `acxstub.lib` (targets ACX 1.1).
#[unsafe(no_mangle)]
pub static AcxMinimumVersionRequired: u32 = 1;

/// Stub required by prebuilt `libcore` on `x86_64-pc-windows-msvc` (never called with `panic = "abort"`).
#[unsafe(no_mangle)]
pub extern "C" fn __CxxFrameHandler3() -> i32 {
    0
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe {
        KeBugCheckEx(0x0000_00E2, 0x4150_4558, 0, 0, 0);
    }
}

static mut G_DRIVER: WDFDRIVER = ptr::null_mut();
static mut G_CONTROL_DEVICE: WDFDEVICE = ptr::null_mut();
static mut G_RENDER_CIRCUIT: ACXCIRCUIT = ptr::null_mut();

// SDDL: SYSTEM + Administrators full access; logged-in interactive users read/write (no admin needed to play).
const SDDL_CONTROL: &[u16] = &[
    b'D' as u16, b':' as u16, b'P' as u16,
    b'(' as u16, b'A' as u16, b';' as u16, b';' as u16, b'G' as u16, b'A' as u16, b';' as u16, b';' as u16, b';' as u16, b'S' as u16, b'Y' as u16, b')' as u16,
    b'(' as u16, b'A' as u16, b';' as u16, b';' as u16, b'G' as u16, b'A' as u16, b';' as u16, b';' as u16, b';' as u16, b'B' as u16, b'A' as u16, b')' as u16,
    b'(' as u16, b'A' as u16, b';' as u16, b';' as u16, b'G' as u16, b'R' as u16, b'G' as u16, b'W' as u16, b';' as u16, b';' as u16, b';' as u16, b'I' as u16, b'U' as u16, b')' as u16,
    0,
];
// \Device\ApExclusiveAudio
const DEV_NAME_CONTROL: &[u16] = &[
    b'\\' as u16, b'D' as u16, b'e' as u16, b'v' as u16, b'i' as u16, b'c' as u16, b'e' as u16,
    b'\\' as u16, b'A' as u16, b'p' as u16, b'E' as u16, b'x' as u16, b'c' as u16, b'l' as u16,
    b'u' as u16, b's' as u16, b'i' as u16, b'v' as u16, b'e' as u16, b'A' as u16, b'u' as u16,
    b'd' as u16, b'i' as u16, b'o' as u16, 0,
];
// \DosDevices\Global\ApExclusiveAudio -> \\.\ApExclusiveAudio
const SYMLINK_CONTROL: &[u16] = &[
    b'\\' as u16, b'D' as u16, b'o' as u16, b's' as u16, b'D' as u16, b'e' as u16, b'v' as u16,
    b'i' as u16, b'c' as u16, b'e' as u16, b's' as u16, b'\\' as u16, b'G' as u16, b'l' as u16,
    b'o' as u16, b'b' as u16, b'a' as u16, b'l' as u16, b'\\' as u16, b'A' as u16, b'p' as u16,
    b'E' as u16, b'x' as u16, b'c' as u16, b'l' as u16, b'u' as u16, b's' as u16, b'i' as u16,
    b'v' as u16, b'e' as u16, b'A' as u16, b'u' as u16, b'd' as u16, b'i' as u16, b'o' as u16,
    0,
];
// Must match KSNAME_Render = "Render0" in ap_exclusive_audio.inx
const CIRCUIT_NAME_RENDER: &[u16] = &[
    b'R' as u16, b'e' as u16, b'n' as u16, b'd' as u16, b'e' as u16, b'r' as u16, b'0' as u16, 0,
];

// Supported stereo integer-PCM rates (full 44.1..384 kHz family, ZERO FLOAT!).
const SUPPORTED_RATES: &[u32] = &[
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];
const SUPPORTED_BIT_FORMATS: &[(u16, u16)] = &[(16, 16), (24, 24), (32, 24), (32, 32)];

fn make_ks_pcm_stereo_format(
    rate: u32,
    container_bits: u16,
    valid_bits: u16,
) -> KSDATAFORMAT_WAVEFORMATEXTENSIBLE {
    let channels: u16 = 2;
    let block_align: u16 = channels * (container_bits / 8);
    let avg_bytes_per_sec: u32 = rate * (block_align as u32);
    let mut fmt = KSDATAFORMAT_WAVEFORMATEXTENSIBLE::default();
    fmt.DataFormat.__bindgen_anon_1.FormatSize =
        size_of::<KSDATAFORMAT_WAVEFORMATEXTENSIBLE>() as ULONG;
    fmt.DataFormat.__bindgen_anon_1.Flags = 0;
    fmt.DataFormat.__bindgen_anon_1.SampleSize = 0;
    fmt.DataFormat.__bindgen_anon_1.Reserved = 0;
    fmt.DataFormat.__bindgen_anon_1.MajorFormat = GUID_KSDATAFORMAT_TYPE_AUDIO;
    fmt.DataFormat.__bindgen_anon_1.SubFormat = GUID_KSDATAFORMAT_SUBTYPE_PCM;
    fmt.DataFormat.__bindgen_anon_1.Specifier = GUID_KSDATAFORMAT_SPECIFIER_WAVEFORMATEX;
    fmt.WaveFormatExt.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE as u16;
    fmt.WaveFormatExt.Format.nChannels = channels;
    fmt.WaveFormatExt.Format.nSamplesPerSec = rate;
    fmt.WaveFormatExt.Format.nAvgBytesPerSec = avg_bytes_per_sec;
    fmt.WaveFormatExt.Format.nBlockAlign = block_align;
    fmt.WaveFormatExt.Format.wBitsPerSample = container_bits;
    fmt.WaveFormatExt.Format.cbSize =
        (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16;
    fmt.WaveFormatExt.Samples.wValidBitsPerSample = valid_bits;
    fmt.WaveFormatExt.dwChannelMask = (SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT) as ULONG;
    fmt.WaveFormatExt.SubFormat = GUID_KSDATAFORMAT_SUBTYPE_PCM;
    fmt
}

// ---- Control Device (\\.\ApExclusiveAudio) ----

unsafe extern "C" fn evt_control_create(
    _device: WDFDEVICE,
    request: WDFREQUEST,
    _file_obj: WDFFILEOBJECT,
) {
    unsafe {
        ring::set_reader_connected(true);
        wdf_request_complete(request, STATUS_SUCCESS_VAL);
    }
}

unsafe extern "C" fn evt_control_cleanup(_file_obj: WDFFILEOBJECT) {
    unsafe {
        ring::set_reader_connected(false);
    }
}

unsafe extern "C" fn evt_control_ioctl(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    output_len: usize,
    input_len: usize,
    ioctl_code: ULONG,
) {
    let status;
    let mut info: ULONG_PTR = 0;

    unsafe {
        match ioctl_code {
            IOCTL_AP_GET_STATUS => {
                if output_len < size_of::<ApDriverStatus>() {
                    status = STATUS_BUFFER_TOO_SMALL_VAL;
                } else {
                    let mut out_ptr: *mut c_void = ptr::null_mut();
                    let mut actual_len: usize = 0;
                    let st = wdf_request_retrieve_output_buffer(
                        request,
                        size_of::<ApDriverStatus>(),
                        &mut out_ptr,
                        &mut actual_len,
                    );
                    if nt_success(st) && !out_ptr.is_null() {
                        let snap = ring::get_status_snapshot();
                        ptr::write_unaligned(out_ptr.cast::<ApDriverStatus>(), snap);
                        info = size_of::<ApDriverStatus>() as ULONG_PTR;
                        status = STATUS_SUCCESS_VAL;
                    } else {
                        status = st;
                    }
                }
            }
            IOCTL_AP_READ_PCM => {
                let hdr_size = size_of::<ApPcmChunkHeader>();
                if output_len < hdr_size {
                    status = STATUS_BUFFER_TOO_SMALL_VAL;
                } else {
                    let mut out_ptr: *mut c_void = ptr::null_mut();
                    let mut actual_len: usize = 0;
                    let st = wdf_request_retrieve_output_buffer(
                        request,
                        hdr_size,
                        &mut out_ptr,
                        &mut actual_len,
                    );
                    if nt_success(st) && !out_ptr.is_null() {
                        let max_pcm = actual_len.saturating_sub(hdr_size);
                        let pcm_dst = out_ptr.cast::<u8>().add(hdr_size);
                        let (hdr, copied) = ring::read_pcm_chunk(pcm_dst, max_pcm);
                        ptr::write_unaligned(out_ptr.cast::<ApPcmChunkHeader>(), hdr);
                        info = (hdr_size + copied) as ULONG_PTR;
                        status = STATUS_SUCCESS_VAL;
                    } else {
                        status = st;
                    }
                }
            }
            IOCTL_AP_SET_CONFIG => {
                if input_len < size_of::<ApDriverConfig>() {
                    status = STATUS_BUFFER_TOO_SMALL_VAL;
                } else {
                    let mut in_ptr: *mut c_void = ptr::null_mut();
                    let mut actual_len: usize = 0;
                    let st = wdf_request_retrieve_input_buffer(
                        request,
                        size_of::<ApDriverConfig>(),
                        &mut in_ptr,
                        &mut actual_len,
                    );
                    if nt_success(st) && !in_ptr.is_null() {
                        let cfg = ptr::read_unaligned(in_ptr.cast::<ApDriverConfig>());
                        ring::apply_config(cfg);
                        status = STATUS_SUCCESS_VAL;
                    } else {
                        status = st;
                    }
                }
            }
            IOCTL_AP_RESET_RING => {
                ring::reset_ring_and_counters(false);
                status = STATUS_SUCCESS_VAL;
            }
            _ => {
                status = STATUS_NOT_SUPPORTED_VAL;
            }
        }
        wdf_request_complete_with_information(request, status, info);
    }
}

unsafe fn create_control_device(driver: WDFDRIVER) -> NTSTATUS {
    let sddl = unicode_from_static(SDDL_CONTROL);
    let dev_name = unicode_from_static(DEV_NAME_CONTROL);
    let sym_link = unicode_from_static(SYMLINK_CONTROL);

    unsafe {
        let mut init = wdf_control_device_init_allocate(driver, &sddl);
        if init.is_null() {
            return STATUS_INSUFFICIENT_RESOURCES_VAL;
        }
        let mut st = wdf_device_init_assign_name(init, &dev_name);
        if nt_success(st) {
            wdf_device_init_set_exclusive(init, 1);
            let mut file_cfg = wdf_fileobject_config_init(
                Some(evt_control_create),
                None,
                Some(evt_control_cleanup),
            );
            wdf_device_init_set_file_object_config(init, &mut file_cfg, ptr::null_mut());
        }

        let mut dev: WDFDEVICE = ptr::null_mut();
        if nt_success(st) {
            st = wdf_device_create(&mut init, ptr::null_mut(), &mut dev);
        }
        if !nt_success(st) {
            if !init.is_null() {
                wdf_device_init_free(init);
            }
            return st;
        }

        st = wdf_device_create_symbolic_link(dev, &sym_link);
        if !nt_success(st) {
            wdf_object_delete(dev as WDFOBJECT);
            return st;
        }

        let mut q_cfg = wdf_io_queue_config_init_default(WdfIoQueueDispatchParallel);
        q_cfg.EvtIoDeviceControl = Some(evt_control_ioctl);
        let mut queue: WDFQUEUE = ptr::null_mut();
        st = wdf_io_queue_create(dev, &mut q_cfg, ptr::null_mut(), &mut queue);
        if !nt_success(st) {
            wdf_object_delete(dev as WDFOBJECT);
            return st;
        }

        wdf_control_finish_initializing(dev);
        G_CONTROL_DEVICE = dev;
        STATUS_SUCCESS_VAL
    }
}

// ---- ACX Render Circuit ----

unsafe extern "C" fn evt_circuit_power_up(
    _device: WDFDEVICE,
    _circuit: ACXCIRCUIT,
    _prev: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    STATUS_SUCCESS_VAL
}

unsafe extern "C" fn evt_circuit_power_down(
    _device: WDFDEVICE,
    _circuit: ACXCIRCUIT,
    _target: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    STATUS_SUCCESS_VAL
}

unsafe extern "C" fn evt_pin_set_data_format(_pin: ACXPIN, _format: ACXDATAFORMAT) -> NTSTATUS {
    STATUS_SUCCESS_VAL
}

unsafe fn create_render_circuit(device: WDFDEVICE) -> Result<ACXCIRCUIT, NTSTATUS> {
    unsafe {
        let mut init = acx_circuit_init_allocate(device);
        if init.is_null() {
            return Err(STATUS_INSUFFICIENT_RESOURCES_VAL);
        }

        let name = unicode_from_static(CIRCUIT_NAME_RENDER);
        acx_circuit_init_set_component_id(init, &GUID_AP_EXCL_COMPONENT);
        let _ = acx_circuit_init_assign_name(init, &name);
        acx_circuit_init_set_circuit_type(init, AcxCircuitTypeRender);

        let mut pwr = acx_circuit_pnppower_callbacks_init();
        pwr.EvtAcxCircuitPowerUp = Some(evt_circuit_power_up);
        pwr.EvtAcxCircuitPowerDown = Some(evt_circuit_power_down);
        acx_circuit_init_set_pnppower_callbacks(init, &mut pwr);

        let st = acx_circuit_init_assign_create_stream_callback(
            init,
            Some(stream::evt_circuit_create_stream),
        );
        if !nt_success(st) {
            acx_circuit_init_free(init);
            return Err(st);
        }

        let mut attrs = wdf_object_attributes_init();
        let mut circuit: ACXCIRCUIT = ptr::null_mut();
        let st = acx_circuit_create(device, &mut attrs, &mut init, &mut circuit);
        if !nt_success(st) {
            if !init.is_null() {
                acx_circuit_init_free(init);
            }
            return Err(st);
        }

        let mut pins: [ACXPIN; 2] = [ptr::null_mut(); 2];
        {
            let mut pin_cb = acx_pin_callbacks_init();
            pin_cb.EvtAcxPinSetDataFormat = Some(evt_pin_set_data_format);
            let mut pin_cfg = acx_pin_config_init(0);
            pin_cfg.Type = AcxPinTypeSink;
            pin_cfg.Communication = AcxPinCommunicationSink;
            pin_cfg.Category = &GUID_KSCATEGORY_AUDIO;
            pin_cfg.PinCallbacks = &mut pin_cb;
            pin_cfg.MaxStreams = 1;

            let mut pin_attrs = wdf_object_attributes_init();
            pin_attrs.ParentObject = circuit as WDFOBJECT;
            let st = acx_pin_create(circuit, &mut pin_attrs, &mut pin_cfg, &mut pins[0]);
            if !nt_success(st) {
                return Err(st);
            }
        }
        {
            let mut pin_cb = acx_pin_callbacks_init();
            let mut pin_cfg = acx_pin_config_init(1);
            pin_cfg.Type = AcxPinTypeSource;
            pin_cfg.Communication = AcxPinCommunicationNone;
            pin_cfg.Category = &GUID_KSNODETYPE_LINE_CONNECTOR;
            pin_cfg.PinCallbacks = &mut pin_cb;

            let mut pin_attrs = wdf_object_attributes_init();
            pin_attrs.ParentObject = circuit as WDFOBJECT;
            let st = acx_pin_create(circuit, &mut pin_attrs, &mut pin_cfg, &mut pins[1]);
            if !nt_success(st) {
                return Err(st);
            }
        }
        {
            let mut jack_cfg = acx_jack_config_init();
            jack_cfg.Description.ChannelMapping =
                (SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT) as ULONG;
            jack_cfg.Description.Color = 0;
            jack_cfg.Description.ConnectionType = AcxConnTypeAtapiInternal;
            jack_cfg.Description.GeoLocation = AcxGeoLocFront;
            jack_cfg.Description.GenLocation = AcxGenLocPrimaryBox;
            jack_cfg.Description.PortConnection = AcxPortConnIntegratedDevice;

            let mut jack_attrs = wdf_object_attributes_init();
            jack_attrs.ParentObject = pins[1] as WDFOBJECT;
            let mut jack: ACXJACK = ptr::null_mut();
            let st = acx_jack_create(pins[1], &mut jack_attrs, &mut jack_cfg, &mut jack);
            if !nt_success(st) {
                return Err(st);
            }
            let st = acx_pin_add_jacks(pins[1], &mut jack, 1);
            if !nt_success(st) {
                return Err(st);
            }
        }

        let fmt_list = acx_pin_get_raw_data_format_list(pins[0]);
        if fmt_list.is_null() {
            return Err(STATUS_INSUFFICIENT_RESOURCES_VAL);
        }
        for &rate in SUPPORTED_RATES {
            for &(container_bits, valid_bits) in SUPPORTED_BIT_FORMATS {
                let mut ks_fmt = make_ks_pcm_stereo_format(rate, container_bits, valid_bits);
                let mut fmt_cfg = acx_dataformat_config_init_ks(
                    (&mut ks_fmt as *mut KSDATAFORMAT_WAVEFORMATEXTENSIBLE).cast(),
                );
                let mut fmt_attrs = wdf_object_attributes_init();
                fmt_attrs.ParentObject = circuit as WDFOBJECT;
                let mut acx_fmt: ACXDATAFORMAT = ptr::null_mut();
                let st = acx_dataformat_create(device, &mut fmt_attrs, &mut fmt_cfg, &mut acx_fmt);
                if !nt_success(st) {
                    return Err(st);
                }
                let st = acx_dataformat_list_add(fmt_list, acx_fmt);
                if !nt_success(st) {
                    return Err(st);
                }
            }
        }

        let st = acx_circuit_add_pins(circuit, pins.as_mut_ptr(), 2);
        if !nt_success(st) {
            return Err(st);
        }
        let mut conn = acx_connection_init(circuit as ACXOBJECT, 0, circuit as ACXOBJECT, 1);
        let st = acx_circuit_add_connections(circuit, &mut conn, 1);
        if !nt_success(st) {
            return Err(st);
        }

        Ok(circuit)
    }
}

// ---- PnP Device Lifecycle ----

unsafe extern "C" fn evt_audio_device_cleanup(_obj: WDFOBJECT) {
    unsafe {
        if !G_CONTROL_DEVICE.is_null() {
            let ctrl = G_CONTROL_DEVICE;
            G_CONTROL_DEVICE = ptr::null_mut();
            wdf_object_delete(ctrl as WDFOBJECT);
        }
        G_RENDER_CIRCUIT = ptr::null_mut();
        ring::cleanup_global_ring();
    }
}

unsafe extern "C" fn evt_device_prepare_hardware(
    device: WDFDEVICE,
    _res_raw: WDFCMRESLIST,
    _res_trans: WDFCMRESLIST,
) -> NTSTATUS {
    unsafe {
        if G_RENDER_CIRCUIT.is_null() {
            return STATUS_INVALID_DEVICE_STATE_VAL;
        }
        acx_device_add_circuit(device, G_RENDER_CIRCUIT)
    }
}

unsafe extern "C" fn evt_device_release_hardware(
    device: WDFDEVICE,
    _res_trans: WDFCMRESLIST,
) -> NTSTATUS {
    unsafe {
        if !G_RENDER_CIRCUIT.is_null() {
            return acx_device_remove_circuit(device, G_RENDER_CIRCUIT);
        }
        STATUS_SUCCESS_VAL
    }
}

unsafe extern "C" fn evt_driver_device_add(
    driver: WDFDRIVER,
    mut device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    unsafe {
        if !ring::init_global_ring() {
            return STATUS_INSUFFICIENT_RESOURCES_VAL;
        }

        let mut init_cfg = acx_deviceinit_config_init();
        let st = acx_device_init_initialize(device_init, &mut init_cfg);
        if !nt_success(st) {
            return st;
        }

        let mut pnp = wdf_pnppower_callbacks_init();
        pnp.EvtDevicePrepareHardware = Some(evt_device_prepare_hardware);
        pnp.EvtDeviceReleaseHardware = Some(evt_device_release_hardware);
        wdf_device_init_set_pnppower_callbacks(device_init, &mut pnp);

        let mut attrs = wdf_object_attributes_init();
        attrs.EvtCleanupCallback = Some(evt_audio_device_cleanup);
        let mut device: WDFDEVICE = ptr::null_mut();
        let st = wdf_device_create(&mut device_init, &mut attrs, &mut device);
        if !nt_success(st) {
            return st;
        }

        if G_CONTROL_DEVICE.is_null() {
            let st = create_control_device(driver);
            if !nt_success(st) {
                return st;
            }
        }

        let mut dev_cfg = acx_device_config_init();
        let st = acx_device_initialize(device, &mut dev_cfg);
        if !nt_success(st) {
            return st;
        }

        let mut caps = wdf_device_pnp_caps_init();
        caps.SurpriseRemovalOK = WdfTrue;
        wdf_device_set_pnp_capabilities(device, &mut caps);

        match create_render_circuit(device) {
            Ok(circuit) => {
                G_RENDER_CIRCUIT = circuit;
                STATUS_SUCCESS_VAL
            }
            Err(st) => st,
        }
    }
}

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver_object: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    unsafe {
        stream::init_stream_registry();
        if !ring::init_global_ring() {
            return STATUS_INSUFFICIENT_RESOURCES_VAL;
        }

        let mut wdf_cfg = wdf_driver_config_init(Some(evt_driver_device_add));
        let mut driver: WDFDRIVER = ptr::null_mut();
        let st = wdf_driver_create(
            driver_object,
            registry_path,
            ptr::null_mut(),
            &mut wdf_cfg,
            &mut driver,
        );
        if !nt_success(st) {
            return st;
        }
        G_DRIVER = driver;

        let mut acx_cfg = acx_driver_config_init();
        acx_driver_initialize(driver, &mut acx_cfg)
    }
}
