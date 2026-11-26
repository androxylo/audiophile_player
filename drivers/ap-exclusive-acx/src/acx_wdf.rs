// Inline dispatch wrappers around bindgen-generated KMDF 1.31 & ACX 1.1 function tables.
// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(dead_code)]

use core::ffi::c_void;
use core::mem::{size_of, transmute_copy};

use crate::sys::*;

/// `POOL_FLAG_NON_PAGED` (NonPagedPoolNx) from WDK `wdm.h` (`0x0000000000000040UI64`).
pub const POOL_FLAG_NON_PAGED: u64 = 0x0000_0000_0000_0040;

unsafe extern "system" {
    pub fn PsGetCurrentProcessId() -> *mut c_void;
    pub fn PsGetProcessSessionIdEx(process: PEPROCESS) -> ULONG;
    pub fn IoGetRequestorSessionId(irp: PIRP, p_session_id: *mut ULONG) -> NTSTATUS;
    pub fn KeGetCurrentIrql() -> KIRQL;
}

pub const STATUS_SUCCESS_VAL: NTSTATUS = 0x0000_0000_u32 as i32;
pub const STATUS_NOT_SUPPORTED_VAL: NTSTATUS = 0xC000_00BB_u32 as i32;
pub const STATUS_INVALID_PARAMETER_VAL: NTSTATUS = 0xC000_000D_u32 as i32;
pub const STATUS_DEVICE_CONFIGURATION_ERROR_VAL: NTSTATUS = 0xC000_0182_u32 as i32;
pub const STATUS_INVALID_DEVICE_STATE_VAL: NTSTATUS = 0xC000_0184_u32 as i32;
pub const STATUS_INVALID_STATE_TRANSITION_VAL: NTSTATUS = 0xC000_0185_u32 as i32;
pub const STATUS_INSUFFICIENT_RESOURCES_VAL: NTSTATUS = 0xC000_009A_u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL_VAL: NTSTATUS = 0xC000_0023_u32 as i32;
pub const STATUS_ACCESS_DENIED_VAL: NTSTATUS = 0xC000_0022_u32 as i32;
pub const STATUS_DATA_LATE_ERROR_VAL: NTSTATUS = 0xC000_003D_u32 as i32;
pub const STATUS_DATA_OVERRUN_VAL: NTSTATUS = 0xC000_003C_u32 as i32;

pub const fn nt_success(st: NTSTATUS) -> bool {
    st >= 0
}

pub const POOL_TAG: ULONG = u32::from_le_bytes(*b"xEpA"); // 'ApEx'

// Verified against WDK wdf.h & acx.h:
//   WdfFunctions_01031 : extern const WDFFUNC *WdfFunctions_01031;  (pointer to table)
//   AcxFunctions       : extern ACXFUNC AcxFunctions[];             (table array directly)
unsafe extern "C" {
    pub static WdfFunctions_01031: *const WDFFUNC;
    pub static WdfDriverGlobals: PWDF_DRIVER_GLOBALS;

    pub static AcxFunctions: [ACXFUNC; 221];
    pub static AcxDriverGlobals: PACX_DRIVER_GLOBALS;
}

#[inline(always)]
pub unsafe fn wdf_fn<F>(index: i32) -> F {
    const { assert!(size_of::<F>() == size_of::<WDFFUNC>()) }
    unsafe {
        let fp = *WdfFunctions_01031.add(index as usize);
        transmute_copy::<WDFFUNC, F>(&fp)
    }
}

#[inline(always)]
pub unsafe fn acx_fn<F>(index: i32) -> F {
    const { assert!(size_of::<F>() == size_of::<ACXFUNC>()) }
    unsafe {
        let fp = AcxFunctions[index as usize];
        transmute_copy::<ACXFUNC, F>(&fp)
    }
}

// ---- Standard KS / ACX GUIDs ----

pub const GUID_KSDATAFORMAT_TYPE_AUDIO: GUID = GUID {
    Data1: 0x7364_7561,
    Data2: 0x0000,
    Data3: 0x0010,
    Data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

pub const GUID_KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID {
    Data1: 0x0000_0001,
    Data2: 0x0000,
    Data3: 0x0010,
    Data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
};

pub const GUID_KSDATAFORMAT_SPECIFIER_WAVEFORMATEX: GUID = GUID {
    Data1: 0x0558_9F81,
    Data2: 0xC356,
    Data3: 0x11CE,
    Data4: [0xBF, 0x01, 0x00, 0xAA, 0x00, 0x55, 0x59, 0x5A],
};

pub const GUID_KSCATEGORY_AUDIO: GUID = GUID {
    Data1: 0x6994_AD04,
    Data2: 0x93EF,
    Data3: 0x11D0,
    Data4: [0xA3, 0xCC, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96],
};

pub const GUID_KSNODETYPE_LINE_CONNECTOR: GUID = GUID {
    Data1: 0xDFF2_1FE3,
    Data2: 0xF70F,
    Data3: 0x11D0,
    Data4: [0xB9, 0x17, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96],
};

// Unique vendor component GUID for Audiophile Player Exclusive ACX circuit:
// {6d7f2a84-9b1c-4e65-a23d-8f41c09e31b7}
pub const GUID_AP_EXCL_COMPONENT: GUID = GUID {
    Data1: 0x6D7F_2A84,
    Data2: 0x9B1C,
    Data3: 0x4E65,
    Data4: [0xA2, 0x3D, 0x8F, 0x41, 0xC0, 0x9E, 0x31, 0xB7],
};

pub fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

pub fn guid_to_bytes(g: &GUID) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&g.Data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.Data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.Data3.to_le_bytes());
    out[8..16].copy_from_slice(&g.Data4);
    out
}

pub const fn unicode_from_static(buf: &'static [u16]) -> UNICODE_STRING {
    let chars_without_nul = buf.len() - 1;
    UNICODE_STRING {
        Length: (chars_without_nul * 2) as u16,
        MaximumLength: (buf.len() * 2) as u16,
        Buffer: buf.as_ptr() as *mut u16,
    }
}

// ---- WDF / ACX Struct Initializers ----

pub fn wdf_object_attributes_init() -> WDF_OBJECT_ATTRIBUTES {
    let mut a = WDF_OBJECT_ATTRIBUTES::default();
    a.Size = size_of::<WDF_OBJECT_ATTRIBUTES>() as ULONG;
    a.ExecutionLevel = WdfExecutionLevelInheritFromParent;
    a.SynchronizationScope = WdfSynchronizationScopeInheritFromParent;
    a
}

pub fn wdf_driver_config_init(evt_device_add: PFN_WDF_DRIVER_DEVICE_ADD) -> WDF_DRIVER_CONFIG {
    let mut cfg = WDF_DRIVER_CONFIG::default();
    cfg.Size = size_of::<WDF_DRIVER_CONFIG>() as ULONG;
    cfg.EvtDriverDeviceAdd = evt_device_add;
    cfg
}

pub fn wdf_pnppower_callbacks_init() -> WDF_PNPPOWER_EVENT_CALLBACKS {
    let mut cb = WDF_PNPPOWER_EVENT_CALLBACKS::default();
    cb.Size = size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG;
    cb
}

pub fn wdf_device_pnp_caps_init() -> WDF_DEVICE_PNP_CAPABILITIES {
    let mut caps = WDF_DEVICE_PNP_CAPABILITIES::default();
    caps.Size = size_of::<WDF_DEVICE_PNP_CAPABILITIES>() as ULONG;
    caps.LockSupported = WdfUseDefault;
    caps.EjectSupported = WdfUseDefault;
    caps.Removable = WdfUseDefault;
    caps.DockDevice = WdfUseDefault;
    caps.UniqueID = WdfUseDefault;
    caps.SilentInstall = WdfUseDefault;
    caps.SurpriseRemovalOK = WdfUseDefault;
    caps.HardwareDisabled = WdfUseDefault;
    caps.NoDisplayInUI = WdfUseDefault;
    caps.Address = u32::MAX;
    caps.UINumber = u32::MAX;
    caps
}

pub fn wdf_fileobject_config_init(
    evt_create: PFN_WDF_DEVICE_FILE_CREATE,
    evt_close: PFN_WDF_FILE_CLOSE,
    evt_cleanup: PFN_WDF_FILE_CLEANUP,
) -> WDF_FILEOBJECT_CONFIG {
    let mut cfg = WDF_FILEOBJECT_CONFIG::default();
    cfg.Size = size_of::<WDF_FILEOBJECT_CONFIG>() as ULONG;
    cfg.EvtDeviceFileCreate = evt_create;
    cfg.EvtFileClose = evt_close;
    cfg.EvtFileCleanup = evt_cleanup;
    cfg.FileObjectClass = WdfFileObjectWdfCannotUseFsContexts;
    cfg.AutoForwardCleanupClose = WdfUseDefault;
    cfg
}

pub fn wdf_io_queue_config_init_default(dispatch: WDF_IO_QUEUE_DISPATCH_TYPE) -> WDF_IO_QUEUE_CONFIG {
    let mut cfg = WDF_IO_QUEUE_CONFIG::default();
    cfg.Size = size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG;
    cfg.PowerManaged = WdfUseDefault;
    cfg.DefaultQueue = 1;
    cfg.DispatchType = dispatch;
    cfg.Settings.Parallel.NumberOfPresentedRequests = u32::MAX;
    cfg
}

pub fn wdf_timer_config_init(evt_timer: PFN_WDF_TIMER) -> WDF_TIMER_CONFIG {
    let mut cfg = WDF_TIMER_CONFIG::default();
    cfg.Size = size_of::<WDF_TIMER_CONFIG>() as ULONG;
    cfg.EvtTimerFunc = evt_timer;
    cfg.AutomaticSerialization = 0;
    cfg.UseHighResolutionTimer = WdfTrue as u8;
    cfg
}

pub fn wdf_memory_descriptor_init_mdl(mdl: PMDL, byte_count: ULONG) -> WDF_MEMORY_DESCRIPTOR {
    let mut desc = WDF_MEMORY_DESCRIPTOR::default();
    desc.Type = WdfMemoryDescriptorTypeMdl;
    desc.u.MdlType.Mdl = mdl;
    desc.u.MdlType.BufferLength = byte_count;
    desc
}

pub fn acx_driver_config_init() -> ACX_DRIVER_CONFIG {
    ACX_DRIVER_CONFIG {
        Size: size_of::<ACX_DRIVER_CONFIG>() as ULONG,
        Flags: AcxDriverConfigNoFlags as ULONG,
    }
}

pub fn acx_deviceinit_config_init() -> ACX_DEVICEINIT_CONFIG {
    let mut cfg = ACX_DEVICEINIT_CONFIG::default();
    cfg.Size = size_of::<ACX_DEVICEINIT_CONFIG>() as ULONG;
    cfg.Flags = AcxDeviceInitConfigNoFlags as ULONG;
    cfg.SynchronizationScope = WdfSynchronizationScopeNone;
    cfg.ExecutionLevel = WdfExecutionLevelPassive;
    cfg
}

pub fn acx_device_config_init() -> ACX_DEVICE_CONFIG {
    let mut cfg = ACX_DEVICE_CONFIG::default();
    cfg.Size = size_of::<ACX_DEVICE_CONFIG>() as ULONG;
    cfg.Flags = AcxDeviceConfigNoFlags as ULONG;
    cfg
}

pub fn acx_circuit_pnppower_callbacks_init() -> ACX_CIRCUIT_PNPPOWER_CALLBACKS {
    let mut cb = ACX_CIRCUIT_PNPPOWER_CALLBACKS::default();
    cb.Size = size_of::<ACX_CIRCUIT_PNPPOWER_CALLBACKS>() as ULONG;
    cb
}

pub fn acx_pin_callbacks_init() -> ACX_PIN_CALLBACKS {
    let mut cb = ACX_PIN_CALLBACKS::default();
    cb.Size = size_of::<ACX_PIN_CALLBACKS>() as ULONG;
    cb
}

pub fn acx_pin_config_init(id: ULONG) -> ACX_PIN_CONFIG {
    let mut cfg = ACX_PIN_CONFIG::default();
    cfg.Size = size_of::<ACX_PIN_CONFIG>() as ULONG;
    cfg.Id = id;
    cfg.Flags = AcxPinConfigNoFlags as ULONG;
    cfg.MaxStreams = u32::MAX;
    cfg
}

pub fn acx_jack_config_init() -> ACX_JACK_CONFIG {
    let mut cfg = ACX_JACK_CONFIG::default();
    cfg.Size = size_of::<ACX_JACK_CONFIG>() as ULONG;
    cfg.Id = u32::MAX;
    cfg.Flags = AcxJackConfigNoFlags as ULONG;
    cfg
}

pub fn acx_dataformat_config_init_ks(ks_format: PVOID) -> ACX_DATAFORMAT_CONFIG {
    let mut cfg = ACX_DATAFORMAT_CONFIG::default();
    cfg.Size = size_of::<ACX_DATAFORMAT_CONFIG>() as ULONG;
    cfg.Flags = AcxDataFormatConfigNoFlags as ULONG;
    cfg.Type = AcxDataFormatKsFormat;
    cfg.u.KsFormat = ks_format;
    cfg
}

pub fn acx_connection_init(
    from_obj: ACXOBJECT,
    from_pin_id: ULONG,
    to_obj: ACXOBJECT,
    to_pin_id: ULONG,
) -> ACX_CONNECTION {
    let mut conn = ACX_CONNECTION::default();
    conn.Size = size_of::<ACX_CONNECTION>() as ULONG;
    conn.Flags = AcxConnectionNoFlags as ULONG;
    conn.FromObject = from_obj;
    conn.FromPin.Id = from_pin_id;
    conn.ToObject = to_obj;
    conn.ToPin.Id = to_pin_id;
    conn
}

pub fn acx_stream_callbacks_init() -> ACX_STREAM_CALLBACKS {
    let mut cb = ACX_STREAM_CALLBACKS::default();
    cb.Size = size_of::<ACX_STREAM_CALLBACKS>() as ULONG;
    cb
}

pub fn acx_rt_stream_callbacks_init() -> ACX_RT_STREAM_CALLBACKS {
    let mut cb = ACX_RT_STREAM_CALLBACKS::default();
    cb.Size = size_of::<ACX_RT_STREAM_CALLBACKS>() as ULONG;
    cb
}

pub fn acx_rtpacket_init() -> ACX_RTPACKET {
    let mut pkt = ACX_RTPACKET::default();
    pkt.Size = size_of::<ACX_RTPACKET>() as ULONG;
    pkt
}

// ---- WDF Wrappers ----

pub unsafe fn wdf_driver_create(
    driver_object: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    config: PWDF_DRIVER_CONFIG,
    driver: *mut WDFDRIVER,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFDRIVERCREATE = wdf_fn(WdfDriverCreateTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, driver_object, registry_path, attributes, config, driver)
    }
}

pub unsafe fn wdf_device_create(
    device_init: *mut PWDFDEVICE_INIT,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    device: *mut WDFDEVICE,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFDEVICECREATE = wdf_fn(WdfDeviceCreateTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device_init, attributes, device)
    }
}

pub unsafe fn wdf_control_device_init_allocate(
    driver: WDFDRIVER,
    sddl: PCUNICODE_STRING,
) -> PWDFDEVICE_INIT {
    unsafe {
        let f: PFN_WDFCONTROLDEVICEINITALLOCATE = wdf_fn(WdfControlDeviceInitAllocateTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, driver, sddl)
    }
}

pub unsafe fn wdf_device_init_assign_name(
    device_init: PWDFDEVICE_INIT,
    device_name: PCUNICODE_STRING,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFDEVICEINITASSIGNNAME = wdf_fn(WdfDeviceInitAssignNameTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device_init, device_name)
    }
}

pub unsafe fn wdf_device_init_set_exclusive(device_init: PWDFDEVICE_INIT, exclusive: BOOLEAN) {
    unsafe {
        let f: PFN_WDFDEVICEINITSETEXCLUSIVE = wdf_fn(WdfDeviceInitSetExclusiveTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device_init, exclusive);
    }
}

pub unsafe fn wdf_device_init_set_file_object_config(
    device_init: PWDFDEVICE_INIT,
    config: PWDF_FILEOBJECT_CONFIG,
    attributes: PWDF_OBJECT_ATTRIBUTES,
) {
    unsafe {
        let f: PFN_WDFDEVICEINITSETFILEOBJECTCONFIG =
            wdf_fn(WdfDeviceInitSetFileObjectConfigTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device_init, config, attributes);
    }
}

pub unsafe fn wdf_device_init_set_pnppower_callbacks(
    device_init: PWDFDEVICE_INIT,
    callbacks: PWDF_PNPPOWER_EVENT_CALLBACKS,
) {
    unsafe {
        let f: PFN_WDFDEVICEINITSETPNPPOWEREVENTCALLBACKS =
            wdf_fn(WdfDeviceInitSetPnpPowerEventCallbacksTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device_init, callbacks);
    }
}

pub unsafe fn wdf_device_init_free(device_init: PWDFDEVICE_INIT) {
    unsafe {
        let f: PFN_WDFDEVICEINITFREE = wdf_fn(WdfDeviceInitFreeTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device_init);
    }
}

pub unsafe fn wdf_device_create_symbolic_link(
    device: WDFDEVICE,
    sym_link: PCUNICODE_STRING,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFDEVICECREATESYMBOLICLINK = wdf_fn(WdfDeviceCreateSymbolicLinkTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device, sym_link)
    }
}

pub unsafe fn wdf_io_queue_create(
    device: WDFDEVICE,
    config: PWDF_IO_QUEUE_CONFIG,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    queue: *mut WDFQUEUE,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFIOQUEUECREATE = wdf_fn(WdfIoQueueCreateTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device, config, attributes, queue)
    }
}

pub unsafe fn wdf_control_finish_initializing(device: WDFDEVICE) {
    unsafe {
        let f: PFN_WDFCONTROLFINISHINITIALIZING = wdf_fn(WdfControlFinishInitializingTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device);
    }
}

pub unsafe fn wdf_device_set_pnp_capabilities(
    device: WDFDEVICE,
    caps: PWDF_DEVICE_PNP_CAPABILITIES,
) {
    unsafe {
        let f: PFN_WDFDEVICESETPNPCAPABILITIES = wdf_fn(WdfDeviceSetPnpCapabilitiesTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, device, caps);
    }
}

pub unsafe fn wdf_object_delete(object: WDFOBJECT) {
    unsafe {
        let f: PFN_WDFOBJECTDELETE = wdf_fn(WdfObjectDeleteTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, object);
    }
}

pub unsafe fn wdf_request_wdm_get_irp(request: WDFREQUEST) -> PIRP {
    unsafe {
        let f: PFN_WDFREQUESTWDMGETIRP = wdf_fn(WdfRequestWdmGetIrpTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, request)
    }
}

pub unsafe fn wdf_request_complete(request: WDFREQUEST, status: NTSTATUS) {
    unsafe {
        let f: PFN_WDFREQUESTCOMPLETE = wdf_fn(WdfRequestCompleteTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, request, status);
    }
}

pub unsafe fn wdf_request_complete_with_information(
    request: WDFREQUEST,
    status: NTSTATUS,
    info: ULONG_PTR,
) {
    unsafe {
        let f: PFN_WDFREQUESTCOMPLETEWITHINFORMATION =
            wdf_fn(WdfRequestCompleteWithInformationTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, request, status, info);
    }
}

pub unsafe fn wdf_request_retrieve_input_buffer(
    request: WDFREQUEST,
    min_size: usize,
    buffer: *mut *mut c_void,
    length: *mut usize,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFREQUESTRETRIEVEINPUTBUFFER = wdf_fn(WdfRequestRetrieveInputBufferTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, request, min_size, buffer, length)
    }
}

pub unsafe fn wdf_request_retrieve_output_buffer(
    request: WDFREQUEST,
    min_size: usize,
    buffer: *mut *mut c_void,
    length: *mut usize,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFREQUESTRETRIEVEOUTPUTBUFFER = wdf_fn(WdfRequestRetrieveOutputBufferTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, request, min_size, buffer, length)
    }
}

pub unsafe fn wdf_timer_create(
    config: PWDF_TIMER_CONFIG,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    timer: *mut WDFTIMER,
) -> NTSTATUS {
    unsafe {
        let f: PFN_WDFTIMERCREATE = wdf_fn(WdfTimerCreateTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, config, attributes, timer)
    }
}

pub unsafe fn wdf_timer_start(timer: WDFTIMER, due_time: LONGLONG) -> BOOLEAN {
    unsafe {
        let f: PFN_WDFTIMERSTART = wdf_fn(WdfTimerStartTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, timer, due_time)
    }
}

pub unsafe fn wdf_timer_stop(timer: WDFTIMER, wait: BOOLEAN) -> BOOLEAN {
    unsafe {
        let f: PFN_WDFTIMERSTOP = wdf_fn(WdfTimerStopTableIndex);
        f.unwrap_unchecked()(WdfDriverGlobals, timer, wait)
    }
}

// ---- ACX Wrappers ----

pub unsafe fn acx_driver_initialize(driver: WDFDRIVER, config: PACX_DRIVER_CONFIG) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDRIVERINITIALIZE = acx_fn(AcxDriverInitializeTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, driver, config)
    }
}

pub unsafe fn acx_device_init_initialize(
    device_init: PWDFDEVICE_INIT,
    config: PACX_DEVICEINIT_CONFIG,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDEVICEINITINITIALIZE = acx_fn(AcxDeviceInitInitializeTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device_init, config)
    }
}

pub unsafe fn acx_device_initialize(device: WDFDEVICE, config: PACX_DEVICE_CONFIG) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDEVICEINITIALIZE = acx_fn(AcxDeviceInitializeTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device, config)
    }
}

pub unsafe fn acx_device_add_circuit(device: WDFDEVICE, circuit: ACXCIRCUIT) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDEVICEADDCIRCUIT = acx_fn(AcxDeviceAddCircuitTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device, circuit)
    }
}

pub unsafe fn acx_device_remove_circuit(device: WDFDEVICE, circuit: ACXCIRCUIT) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDEVICEREMOVECIRCUIT = acx_fn(AcxDeviceRemoveCircuitTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device, circuit)
    }
}

pub unsafe fn acx_circuit_init_allocate(device: WDFDEVICE) -> PACXCIRCUIT_INIT {
    unsafe {
        let f: PFN_ACXCIRCUITINITALLOCATE = acx_fn(AcxCircuitInitAllocateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device)
    }
}

pub unsafe fn acx_circuit_init_free(init: PACXCIRCUIT_INIT) {
    unsafe {
        let f: PFN_ACXCIRCUITINITFREE = acx_fn(AcxCircuitInitFreeTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init);
    }
}

pub unsafe fn acx_circuit_init_set_component_id(init: PACXCIRCUIT_INIT, guid: *const GUID) {
    unsafe {
        let f: PFN_ACXCIRCUITINITSETCOMPONENTID = acx_fn(AcxCircuitInitSetComponentIdTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, guid);
    }
}

pub unsafe fn acx_circuit_init_assign_name(
    init: PACXCIRCUIT_INIT,
    name: PCUNICODE_STRING,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXCIRCUITINITASSIGNNAME = acx_fn(AcxCircuitInitAssignNameTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, name)
    }
}

pub unsafe fn acx_circuit_init_set_circuit_type(init: PACXCIRCUIT_INIT, ctype: ACX_CIRCUIT_TYPE) {
    unsafe {
        let f: PFN_ACXCIRCUITINITSETCIRCUITTYPE = acx_fn(AcxCircuitInitSetCircuitTypeTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, ctype);
    }
}

pub unsafe fn acx_circuit_init_set_pnppower_callbacks(
    init: PACXCIRCUIT_INIT,
    callbacks: PACX_CIRCUIT_PNPPOWER_CALLBACKS,
) {
    unsafe {
        let f: PFN_ACXCIRCUITINITSETACXCIRCUITPNPPOWERCALLBACKS =
            acx_fn(AcxCircuitInitSetAcxCircuitPnpPowerCallbacksTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, callbacks);
    }
}

pub unsafe fn acx_circuit_init_assign_create_stream_callback(
    init: PACXCIRCUIT_INIT,
    cb: PFN_ACX_CIRCUIT_CREATE_STREAM,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXCIRCUITINITASSIGNACXCREATESTREAMCALLBACK =
            acx_fn(AcxCircuitInitAssignAcxCreateStreamCallbackTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, cb)
    }
}

pub unsafe fn acx_circuit_create(
    device: WDFDEVICE,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    init: *mut PACXCIRCUIT_INIT,
    circuit: *mut ACXCIRCUIT,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXCIRCUITCREATE = acx_fn(AcxCircuitCreateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device, attributes, init, circuit)
    }
}

pub unsafe fn acx_pin_create(
    circuit: ACXCIRCUIT,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    config: PACX_PIN_CONFIG,
    pin: *mut ACXPIN,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXPINCREATE = acx_fn(AcxPinCreateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, circuit, attributes, config, pin)
    }
}

pub unsafe fn acx_pin_get_raw_data_format_list(pin: ACXPIN) -> ACXDATAFORMATLIST {
    unsafe {
        let f: PFN_ACXPINGETRAWDATAFORMATLIST = acx_fn(AcxPinGetRawDataFormatListTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, pin)
    }
}

pub unsafe fn acx_pin_add_jacks(pin: ACXPIN, jacks: *mut ACXJACK, count: ULONG) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXPINADDJACKS = acx_fn(AcxPinAddJacksTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, pin, jacks, count)
    }
}

pub unsafe fn acx_jack_create(
    pin: ACXPIN,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    config: PACX_JACK_CONFIG,
    jack: *mut ACXJACK,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXJACKCREATE = acx_fn(AcxJackCreateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, pin, attributes, config, jack)
    }
}

pub unsafe fn acx_dataformat_create(
    device: WDFDEVICE,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    config: PACX_DATAFORMAT_CONFIG,
    format: *mut ACXDATAFORMAT,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDATAFORMATCREATE = acx_fn(AcxDataFormatCreateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device, attributes, config, format)
    }
}

pub unsafe fn acx_dataformat_list_add(list: ACXDATAFORMATLIST, format: ACXDATAFORMAT) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXDATAFORMATLISTADDDATAFORMAT =
            acx_fn(AcxDataFormatListAddDataFormatTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, list, format)
    }
}

pub unsafe fn acx_circuit_add_pins(
    circuit: ACXCIRCUIT,
    pins: *mut ACXPIN,
    count: ULONG,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXCIRCUITADDPINS = acx_fn(AcxCircuitAddPinsTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, circuit, pins, count)
    }
}

pub unsafe fn acx_circuit_add_connections(
    circuit: ACXCIRCUIT,
    connections: PACX_CONNECTION,
    count: ULONG,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXCIRCUITADDCONNECTIONS = acx_fn(AcxCircuitAddConnectionsTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, circuit, connections, count)
    }
}

pub unsafe fn acx_stream_init_assign_stream_callbacks(
    init: PACXSTREAM_INIT,
    callbacks: PACX_STREAM_CALLBACKS,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXSTREAMINITASSIGNACXSTREAMCALLBACKS =
            acx_fn(AcxStreamInitAssignAcxStreamCallbacksTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, callbacks)
    }
}

pub unsafe fn acx_stream_init_assign_rt_stream_callbacks(
    init: PACXSTREAM_INIT,
    callbacks: PACX_RT_STREAM_CALLBACKS,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXSTREAMINITASSIGNACXRTSTREAMCALLBACKS =
            acx_fn(AcxStreamInitAssignAcxRtStreamCallbacksTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init, callbacks)
    }
}

pub unsafe fn acx_stream_init_set_rt_supports_notifications(init: PACXSTREAM_INIT) {
    unsafe {
        let f: PFN_ACXSTREAMINITSETACXRTSTREAMSUPPORTSNOTIFICATIONS =
            acx_fn(AcxStreamInitSetAcxRtStreamSupportsNotificationsTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, init);
    }
}

pub unsafe fn acx_rt_stream_create(
    device: WDFDEVICE,
    circuit: ACXCIRCUIT,
    attributes: PWDF_OBJECT_ATTRIBUTES,
    init: *mut PACXSTREAM_INIT,
    stream: *mut ACXSTREAM,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXRTSTREAMCREATE = acx_fn(AcxRtStreamCreateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, device, circuit, attributes, init, stream)
    }
}

pub unsafe fn acx_rt_stream_notify_packet_complete(
    stream: ACXSTREAM,
    completed_packet: ULONGLONG,
    qpc_completion: ULONGLONG,
) -> NTSTATUS {
    unsafe {
        let f: PFN_ACXRTSTREAMNOTIFYPACKETCOMPLETE =
            acx_fn(AcxRtStreamNotifyPacketCompleteTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, stream, completed_packet, qpc_completion)
    }
}

pub unsafe fn acx_dataformat_get_sample_rate(fmt: ACXDATAFORMAT) -> ULONG {
    unsafe {
        let f: PFN_ACXDATAFORMATGETSAMPLERATE = acx_fn(AcxDataFormatGetSampleRateTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}

pub unsafe fn acx_dataformat_get_channels(fmt: ACXDATAFORMAT) -> USHORT {
    unsafe {
        let f: PFN_ACXDATAFORMATGETCHANNELSCOUNT = acx_fn(AcxDataFormatGetChannelsCountTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}

pub unsafe fn acx_dataformat_get_bits_per_sample(fmt: ACXDATAFORMAT) -> ULONG {
    unsafe {
        let f: PFN_ACXDATAFORMATGETBITSPERSAMPLE = acx_fn(AcxDataFormatGetBitsPerSampleTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}

pub unsafe fn acx_dataformat_get_valid_bits_per_sample(fmt: ACXDATAFORMAT) -> ULONG {
    unsafe {
        let f: PFN_ACXDATAFORMATGETVALIDBITSPERSAMPLE =
            acx_fn(AcxDataFormatGetValidBitsPerSampleTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}

pub unsafe fn acx_dataformat_get_block_align(fmt: ACXDATAFORMAT) -> ULONG {
    unsafe {
        let f: PFN_ACXDATAFORMATGETBLOCKALIGN = acx_fn(AcxDataFormatGetBlockAlignTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}

pub unsafe fn acx_dataformat_get_avg_bytes_per_sec(fmt: ACXDATAFORMAT) -> ULONG {
    unsafe {
        let f: PFN_ACXDATAFORMATGETAVERAGEBYTESPERSEC =
            acx_fn(AcxDataFormatGetAverageBytesPerSecTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}

pub unsafe fn acx_dataformat_get_subformat(fmt: ACXDATAFORMAT) -> GUID {
    unsafe {
        let f: PFN_ACXDATAFORMATGETSUBFORMAT = acx_fn(AcxDataFormatGetSubFormatTableIndex);
        f.unwrap_unchecked()(AcxDriverGlobals, fmt)
    }
}
