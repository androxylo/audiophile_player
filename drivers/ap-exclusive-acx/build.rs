// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo
// Additional permission under GPLv3 section 7: see NOTICE for WDK/KMDF/ACX linking.

use std::env;
use std::path::{Path, PathBuf};

fn find_kit_root(manifest_dir: &Path) -> PathBuf {
    for var in ["AP_WDK_KIT_ROOT", "WDKContentRoot"] {
        if let Ok(p) = env::var(var) {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                return pb;
            }
        }
    }
    // Local NuGet WDK kit inside drivers/ap-exclusive-acx/.wdk/kit
    let local = manifest_dir.join(".wdk").join("kit");
    if local.is_dir() {
        return local.canonicalize().unwrap_or(local);
    }
    // Fallback to standard system Windows Driver Kit installation
    let sys_kit = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10");
    if sys_kit.join("Include").is_dir() {
        return sys_kit;
    }
    panic!(
        "Windows Driver Kit not found. Place local WDK kit at {} or install WDK 10.0.26100.",
        local.display()
    );
}

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AP_WDK_KIT_ROOT");
    println!("cargo:rerun-if-env-changed=WDKContentRoot");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let kit = find_kit_root(&manifest_dir);
    let kver = "10.0.26100.0";
    let kmdf = "1.31";
    let acx = "1.1";

    let inc_km = kit.join("Include").join(kver).join("km");
    let inc_km_crt = inc_km.join("crt");
    let inc_um = kit.join("Include").join(kver).join("um");
    let inc_shared = kit.join("Include").join(kver).join("shared");
    let inc_wdf = kit.join("Include").join("wdf").join("kmdf").join(kmdf);
    let inc_acx = inc_km.join("acx").join("km").join(acx);

    let lib_km = kit.join("Lib").join(kver).join("km").join("x64");
    let lib_wdf = kit.join("Lib").join("wdf").join("kmdf").join("x64").join(kmdf);
    let lib_acx = lib_km.join("acx").join("km").join(acx);

    let clean = |p: &Path| -> String {
        let s = p.to_string_lossy().into_owned();
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned()
    };

    let bindings = bindgen::Builder::default()
        .header(clean(&manifest_dir.join("wrapper.h")))
        .use_core()
        .ctypes_prefix("core::ffi")
        .layout_tests(false)
        .derive_default(true)
        .derive_debug(false)
        .prepend_enum_name(false)
        .clang_arg("--target=x86_64-pc-windows-msvc")
        .clang_arg("-fms-extensions")
        .clang_arg("-fms-compatibility")
        .clang_arg("-fms-compatibility-version=19.38")
        .clang_arg("-Wno-everything")
        .clang_arg(format!("-I{}", clean(&inc_km)))
        .clang_arg(format!("-I{}", clean(&inc_km_crt)))
        .clang_arg(format!("-I{}", clean(&inc_shared)))
        .clang_arg(format!("-I{}", clean(&inc_um)))
        .clang_arg(format!("-I{}", clean(&inc_wdf)))
        .clang_arg(format!("-I{}", clean(&inc_acx)))
        .allowlist_type("DRIVER_OBJECT|PDRIVER_OBJECT|UNICODE_STRING|PCUNICODE_STRING|GUID|LARGE_INTEGER|KSPIN_LOCK|KIRQL|PMDL|MDL|PEPROCESS|PIRP|NTSTATUS|ULONG|USHORT|UCHAR|LONG|LONGLONG|ULONGLONG|BOOLEAN|PVOID|SIZE_T")
        .allowlist_type("WAVEFORMATEX|WAVEFORMATEXTENSIBLE|KSDATAFORMAT.*")
        .allowlist_type("WDF.*|PWDF.*|PFN_WDF.*|EVT_WDF.*|_WDFFUNCENUM")
        .allowlist_type("ACX.*|PACX.*|PFN_ACX.*|EVT_ACX.*|_ACXFUNCENUM")
        .allowlist_function("DbgPrintEx|ExAllocatePool2|ExFreePoolWithTag|IoAllocateMdl|IoFreeMdl|MmBuildMdlForNonPagedPool|IoGetCurrentProcess|PsGetCurrentProcessId|PsGetProcessImageFileName|KeInitializeSpinLock|KeAcquireSpinLockRaiseToDpc|KeReleaseSpinLock|KeQueryPerformanceCounter|KeFlushQueuedDpcs|KeBugCheckEx")
        .allowlist_var("WdfFunctions.*|WdfDriverGlobals|AcxFunctions|AcxDriverGlobals|STATUS_.*|POOL_FLAG_.*|PAGE_SIZE|WAVE_FORMAT_.*|KSAUDIO_SPEAKER_.*|SPEAKER_.*|DPFLTR_.*|WDF_NO_.*|ACX_.*|KSSTREAM_HEADER_OPTIONSF_.*")
        .generate()
        .expect("Failed to generate WDK/KMDF/ACX bindings via bindgen");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings.rs");

    println!("cargo:rustc-link-search=native={}", clean(&lib_km));
    println!("cargo:rustc-link-search=native={}", clean(&lib_wdf));
    println!("cargo:rustc-link-search=native={}", clean(&lib_acx));

    println!("cargo:rustc-link-lib=static=WdfDriverEntry");
    println!("cargo:rustc-link-lib=static=WdfLdr");
    println!("cargo:rustc-link-lib=static=acxstub");
    println!("cargo:rustc-link-lib=static=BufferOverflowFastFailK");
    println!("cargo:rustc-link-lib=static=libcntpr");
    println!("cargo:rustc-link-lib=dylib=ntoskrnl");
    println!("cargo:rustc-link-lib=dylib=hal");
    println!("cargo:rustc-link-lib=dylib=wmilib");

    for arg in [
        "/DRIVER",
        "/SUBSYSTEM:NATIVE,10.00",
        "/ENTRY:FxDriverEntry",
        "/NODEFAULTLIB",
        "/MACHINE:X64",
        "/OPT:REF",
        "/OPT:ICF",
        "/RELEASE",
        "/INTEGRITYCHECK",
        "/NXCOMPAT",
        "/DYNAMICBASE",
        "/MERGE:_TEXT=.text;_PAGE=PAGE",
        "/INCLUDE:_Acx_BIND_INFO",
    ] {
        println!("cargo:rustc-cdylib-link-arg={arg}");
    }
}
