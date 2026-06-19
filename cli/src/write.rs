use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use dfu::{DfuConnection, DfuDevice, DfuError, find_dfu_devices};
use uf2::{UF2RangeIterator, is_uf2_payload};

use crate::CliError;

fn intf_name(device: &DfuDevice, interface: u8, alt: u8) -> String {
    device.interfaces()
        .iter()
        .find(|i| i.interface() == interface && i.alt_setting() == alt)
        .map(|i| i.layout().name.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn log_no_memory_segments(device: &DfuDevice, start: u32, end: u32) {
    log::warn!(
        "NoMemorySegments: no interface covers 0x{:08x}-0x{:08x}; available:",
        start, end,
    );
    for intf in device.interfaces() {
        log::warn!(
            "  intf={} alt={} \"{}\"",
            intf.interface(), intf.alt_setting(), intf.layout().name,
        );
        for seg in intf.layout().segments.iter() {
            log::warn!(
                "    0x{:08x}-0x{:08x} r={} e={} w={}",
                seg.start_addr(), seg.end_addr(),
                seg.readable(), seg.erasable(), seg.writable(),
            );
        }
    }
}

pub(crate) fn download(
    data: &[u8],
    device: DfuDevice,
    start_address: Option<u32>,
    reboot_bootloader: bool,
    erase_calibration: bool,
) -> Result<(), CliError> {
    let mut device = device;
    let mut uf2_magic_addr: Option<u32> = None;
    let mut uf2_firmware_addr: Option<u32> = None;
    reset_state(&device)?;
    if !is_uf2_payload(data) {
        download_range(data, &device, start_address)?;
    } else {
        for addr_range in UF2RangeIterator::new(data)? {
            if let Some(reboot_addr) = addr_range.reboot_address {
                log::warn!(
                    "UF2 reboot range: addr=0x{:08x} payload={:?} reboot_addr=0x{:08x}",
                    addr_range.start_address, addr_range.payload, reboot_addr,
                );
                uf2_magic_addr = Some(addr_range.start_address);
                uf2_firmware_addr = Some(reboot_addr);
                device = reboot(
                    &device,
                    addr_range.start_address,
                    &addr_range.payload,
                    reboot_addr,
                )?;
            } else {
                log::warn!(
                    "UF2 data range: addr=0x{:08x} len={}",
                    addr_range.start_address, addr_range.payload.len(),
                );
                download_range(
                    &addr_range.payload,
                    &device,
                    Some(addr_range.start_address),
                )?;
            }
        }
    }
    log::warn!("UF2 loop complete");

    if erase_calibration {
        erase_calibration_segment(&device)?;
    }

    if reboot_bootloader {
        let (magic_addr, bootloader_addr) = match (uf2_magic_addr, uf2_firmware_addr) {
            (Some(m), Some(b)) => (m, b),
            _ => return Err(CliError::Other(
                "--reboot-bootloader requires a UF2 file with a reboot range".to_string(),
            )),
        };
        const BBLD: [u8; 4] = [0x42, 0x42, 0x4C, 0x44];

        match device.find_interface(magic_addr, Some(magic_addr + 3)) {
            Ok(intf) => {
                log::warn!(
                    "Rebooting to bootloader: BBLD→0x{:08x} via intf={} alt={} \"{}\", execute at 0x{:08x}",
                    magic_addr, intf.interface(), intf.alt_setting(), intf.layout().name, bootloader_addr,
                );
                let connection = device.connect(intf.interface(), intf.alt_setting())?;
                connection.reboot(magic_addr, &BBLD, bootloader_addr)?;
            }
            Err(_) => {
                log_no_memory_segments(&device, magic_addr, magic_addr + 3);
                log::warn!(
                    "BBLD write skipped — add DTCM to the EdgeTX DFU memory map \
                     for --reboot-bootloader to work.",
                );
                log::warn!("connect(intf=0, alt=0 \"{}\") for dfuse_leave", intf_name(&device, 0, 0));
                let connection = device.connect(0, 0)?;
                let _ = connection.dfuse_leave(bootloader_addr);
            }
        }
        println!("Jumped to bootloader.");
    } else {
        log::warn!("Calling leave()");
        let result = leave(&device);
        log::warn!("leave() returned: {:?}", result.is_ok());
        result?;
    }
    Ok(())
}

fn erase_pages(connection: &DfuConnection, pages: Vec<u32>) -> Result<(), DfuError> {
    let total = pages.len();
    for (i, page_addr) in pages.into_iter().enumerate() {
        print!("\r  Erasing page {:2} of {:2} @ 0x{:08x}", i + 1, total, page_addr);
        let _ = io::stdout().flush();
        if let Err(err) = connection.dfuse_page_erase(page_addr) {
            println!(" ❌");
            return Err(err);
        }
    }
    println!();
    Ok(())
}

fn erase_calibration_segment(device: &DfuDevice) -> Result<(), CliError> {
    let intf = device
        .interfaces()
        .iter()
        .find(|i| i.layout().name.contains("CALIBFLASH"))
        .ok_or_else(|| {
            log::warn!("CALIBFLASH not found; available interfaces:");
            for i in device.interfaces() {
                log::warn!(
                    "  intf={} alt={} \"{}\"",
                    i.interface(), i.alt_setting(), i.layout().name,
                );
                for seg in i.layout().segments.iter() {
                    log::warn!(
                        "    0x{:08x}-0x{:08x} r={} e={} w={}",
                        seg.start_addr(), seg.end_addr(),
                        seg.readable(), seg.erasable(), seg.writable(),
                    );
                }
            }
            CliError::Other("CALIBFLASH memory segment not found on device".to_string())
        })?;

    println!("Erasing CALIBFLASH...");
    log::warn!(
        "connect(intf={}, alt={} \"{}\") for erase_calibration",
        intf.interface(), intf.alt_setting(), intf.layout().name,
    );
    let connection = device.connect(intf.interface(), intf.alt_setting())?;

    let start = intf.layout().segments.first().start_addr();
    let end = intf.layout().segments.last().end_addr() - 1;
    erase_pages(&connection, intf.get_erase_pages(start, end))?;
    Ok(())
}

pub(crate) fn reset_state(device: &DfuDevice) -> Result<(), DfuError> {
    println!("Resetting device state...");
    log::warn!("connect(intf=0, alt=0 \"{}\") for reset_state", intf_name(device, 0, 0));
    let connection = device.connect(0, 0)?;
    connection.reset_state()
}

fn download_range(
    data: &[u8],
    device: &DfuDevice,
    start_address: Option<u32>,
) -> Result<(), DfuError> {
    let start_address =
        start_address.unwrap_or(device.get_default_start_address());
    let end_address = start_address + (data.len() as u32) - 1;

    let intf = device.find_interface(start_address, Some(end_address))
        .map_err(|e| {
            if matches!(e, DfuError::NoMemorySegments) {
                log_no_memory_segments(device, start_address, end_address);
            }
            e
        })?;
    log::warn!("connect(intf={}, alt={} \"{}\") for download_range 0x{:08x}", intf.interface(), intf.alt_setting(), intf.layout().name, start_address);
    let connection = device.connect(intf.interface(), intf.alt_setting())?;

    erase_pages(&connection, intf.get_erase_pages(start_address, end_address))?;

    let mut addr = start_address;
    let mut bytes_downloaded: usize = 0;
    let transfer_size = connection.transfer_size();

    for chunk in data.chunks(transfer_size as usize) {
        connection.download(addr, chunk)?;
        addr += chunk.len() as u32;
        bytes_downloaded += chunk.len();

        let percentage = (100 * bytes_downloaded) / data.len();
        let filled = (60 * bytes_downloaded) / data.len();
        print!(
            "\r  Flashing {:3}% [{}]",
            percentage,
            "#".repeat(filled) + &" ".repeat(60 - filled)
        );
        let _ = io::stdout().flush();
    }
    println!();

    Ok(())
}

fn reboot(
    device: &DfuDevice,
    addr: u32,
    payload: &[u8],
    reboot_addr: u32,
) -> Result<DfuDevice, DfuError> {
    log::warn!("connect(intf=0, alt=0 \"{}\") for reboot addr=0x{:08x}", intf_name(device, 0, 0), addr);
    let connection = device.connect(0, 0)?;
    connection.reboot(addr, payload, reboot_addr)?;
    drop(connection);

    println!("Waiting for device to reconnect...");
    let start = Instant::now();
    loop {
        let devices = find_dfu_devices(
            Some(device.vendor_id()),
            Some(device.product_id()),
        )?;
        if !devices.is_empty() {
            println!("Device reconnected");
            return Ok(devices.into_iter().next().unwrap());
        }
        if start.elapsed() >= Duration::from_secs(30) {
            return Err(DfuError::Timeout);
        }
    }
}

fn leave(device: &DfuDevice) -> Result<(), DfuError> {
    println!("Leaving DFU...");
    log::warn!("connect(intf=0, alt=0 \"{}\") for leave", intf_name(device, 0, 0));
    let connection = device.connect(0, 0)?;
    connection.leave()
}
