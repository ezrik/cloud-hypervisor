// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

extern crate devices;
extern crate pci;
extern crate vm_allocator;
extern crate vm_memory;
extern crate vmm_sys_util;

use byteorder::{ByteOrder, LittleEndian};
use libc::EFD_NONBLOCK;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use devices::BusDevice;
use pci::{
    PciBarConfiguration, PciCapability, PciCapabilityID, PciClassCode, PciConfiguration, PciDevice,
    PciDeviceError, PciHeaderType, PciInterruptPin, PciSubclass,
};
use vm_allocator::SystemAllocator;
use vm_memory::{Address, ByteValued, GuestAddress, GuestMemoryMmap, GuestUsize, Le32};
use vmm_sys_util::{EventFd, Result};

use super::VirtioPciCommonConfig;
use crate::{
    Queue, VirtioDevice, DEVICE_ACKNOWLEDGE, DEVICE_DRIVER, DEVICE_DRIVER_OK, DEVICE_FAILED,
    DEVICE_FEATURES_OK, DEVICE_INIT,
};

#[allow(clippy::enum_variant_names)]
enum PciCapabilityType {
    CommonConfig = 1,
    NotifyConfig = 2,
    IsrConfig = 3,
    DeviceConfig = 4,
    PciConfig = 5,
}

#[allow(dead_code)]
#[repr(packed)]
#[derive(Clone, Copy, Default)]
struct VirtioPciCap {
    cap_len: u8,      // Generic PCI field: capability length
    cfg_type: u8,     // Identifies the structure.
    pci_bar: u8,      // Where to find it.
    padding: [u8; 3], // Pad to full dword.
    offset: Le32,     // Offset within bar.
    length: Le32,     // Length of the structure, in bytes.
}
// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
unsafe impl ByteValued for VirtioPciCap {}

impl PciCapability for VirtioPciCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

const VIRTIO_PCI_CAPABILITY_BYTES: u8 = 16;

impl VirtioPciCap {
    pub fn new(cfg_type: PciCapabilityType, pci_bar: u8, offset: u32, length: u32) -> Self {
        VirtioPciCap {
            cap_len: VIRTIO_PCI_CAPABILITY_BYTES,
            cfg_type: cfg_type as u8,
            pci_bar,
            padding: [0; 3],
            offset: Le32::from(offset),
            length: Le32::from(length),
        }
    }
}

#[allow(dead_code)]
#[repr(packed)]
#[derive(Clone, Copy, Default)]
struct VirtioPciNotifyCap {
    cap: VirtioPciCap,
    notify_off_multiplier: Le32,
}
// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
unsafe impl ByteValued for VirtioPciNotifyCap {}

impl PciCapability for VirtioPciNotifyCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

impl VirtioPciNotifyCap {
    pub fn new(
        cfg_type: PciCapabilityType,
        pci_bar: u8,
        offset: u32,
        length: u32,
        multiplier: Le32,
    ) -> Self {
        VirtioPciNotifyCap {
            cap: VirtioPciCap {
                cap_len: std::mem::size_of::<VirtioPciNotifyCap>() as u8,
                cfg_type: cfg_type as u8,
                pci_bar,
                padding: [0; 3],
                offset: Le32::from(offset),
                length: Le32::from(length),
            },
            notify_off_multiplier: multiplier,
        }
    }
}

#[allow(dead_code)]
#[derive(Copy, Clone)]
pub enum PciVirtioSubclass {
    NonTransitionalBase = 0xff,
}

impl PciSubclass for PciVirtioSubclass {
    fn get_register_value(&self) -> u8 {
        *self as u8
    }
}

// Allocate one bar for the structs pointed to by the capability structures.
const COMMON_CONFIG_BAR_OFFSET: u64 = 0x0000;
const COMMON_CONFIG_SIZE: u64 = 56;
const ISR_CONFIG_BAR_OFFSET: u64 = 0x1000;
const ISR_CONFIG_SIZE: u64 = 1;
const DEVICE_CONFIG_BAR_OFFSET: u64 = 0x2000;
const DEVICE_CONFIG_SIZE: u64 = 0x1000;
const NOTIFICATION_BAR_OFFSET: u64 = 0x3000;
const NOTIFICATION_SIZE: u64 = 0x1000;
const CAPABILITY_BAR_SIZE: u64 = 0x4000;

const NOTIFY_OFF_MULTIPLIER: u32 = 4; // A dword per notification address.

const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040; // Add to device type to get device ID.

pub struct VirtioPciDevice {
    // PCI configuration registers.
    configuration: PciConfiguration,

    // virtio PCI common configuration
    common_config: VirtioPciCommonConfig,

    // Virtio device reference and status
    device: Box<VirtioDevice>,
    device_activated: bool,

    // PCI interrupts.
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: Option<EventFd>,

    // virtio queues
    queues: Vec<Queue>,
    queue_evts: Vec<EventFd>,

    // Guest memory
    memory: Option<GuestMemoryMmap>,

    // Setting PCI BAR
    settings_bar: u8,
}

impl VirtioPciDevice {
    /// Constructs a new PCI transport for the given virtio device.
    pub fn new(memory: GuestMemoryMmap, device: Box<VirtioDevice>) -> Result<Self> {
        let mut queue_evts = Vec::new();
        for _ in device.queue_max_sizes().iter() {
            queue_evts.push(EventFd::new(EFD_NONBLOCK)?)
        }
        let queues = device
            .queue_max_sizes()
            .iter()
            .map(|&s| Queue::new(s))
            .collect();

        let pci_device_id = VIRTIO_PCI_DEVICE_ID_BASE + device.device_type() as u16;

        let configuration = PciConfiguration::new(
            VIRTIO_PCI_VENDOR_ID,
            pci_device_id,
            PciClassCode::Other,
            &PciVirtioSubclass::NonTransitionalBase,
            None,
            PciHeaderType::Device,
            VIRTIO_PCI_VENDOR_ID,
            pci_device_id,
        );

        Ok(VirtioPciDevice {
            configuration,
            common_config: VirtioPciCommonConfig {
                driver_status: 0,
                config_generation: 0,
                device_feature_select: 0,
                driver_feature_select: 0,
                queue_select: 0,
            },
            device,
            device_activated: false,
            interrupt_status: Arc::new(AtomicUsize::new(0)),
            interrupt_evt: None,
            queues,
            queue_evts,
            memory: Some(memory),
            settings_bar: 0,
        })
    }

    /// Gets the list of queue events that must be triggered whenever the VM writes to
    /// `virtio::NOTIFY_REG_OFFSET` past the MMIO base. Each event must be triggered when the
    /// value being written equals the index of the event in this list.
    pub fn queue_evts(&self) -> &[EventFd] {
        self.queue_evts.as_slice()
    }

    /// Gets the event this device uses to interrupt the VM when the used queue is changed.
    pub fn interrupt_evt(&self) -> Option<&EventFd> {
        self.interrupt_evt.as_ref()
    }

    fn is_driver_ready(&self) -> bool {
        let ready_bits =
            (DEVICE_ACKNOWLEDGE | DEVICE_DRIVER | DEVICE_DRIVER_OK | DEVICE_FEATURES_OK) as u8;
        self.common_config.driver_status == ready_bits
            && self.common_config.driver_status & DEVICE_FAILED as u8 == 0
    }

    /// Determines if the driver has requested the device (re)init / reset itself
    fn is_driver_init(&self) -> bool {
        self.common_config.driver_status == DEVICE_INIT as u8
    }

    fn are_queues_valid(&self) -> bool {
        if let Some(mem) = self.memory.as_ref() {
            self.queues.iter().all(|q| q.is_valid(mem))
        } else {
            false
        }
    }

    fn add_pci_capabilities(
        &mut self,
        settings_bar: u8,
    ) -> std::result::Result<(), PciDeviceError> {
        // Add pointers to the different configuration structures from the PCI capabilities.
        let common_cap = VirtioPciCap::new(
            PciCapabilityType::CommonConfig,
            settings_bar,
            COMMON_CONFIG_BAR_OFFSET as u32,
            COMMON_CONFIG_SIZE as u32,
        );
        self.configuration
            .add_capability(&common_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let isr_cap = VirtioPciCap::new(
            PciCapabilityType::IsrConfig,
            settings_bar,
            ISR_CONFIG_BAR_OFFSET as u32,
            ISR_CONFIG_SIZE as u32,
        );
        self.configuration
            .add_capability(&isr_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        // TODO(dgreid) - set based on device's configuration size?
        let device_cap = VirtioPciCap::new(
            PciCapabilityType::DeviceConfig,
            settings_bar,
            DEVICE_CONFIG_BAR_OFFSET as u32,
            DEVICE_CONFIG_SIZE as u32,
        );
        self.configuration
            .add_capability(&device_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let notify_cap = VirtioPciNotifyCap::new(
            PciCapabilityType::NotifyConfig,
            settings_bar,
            NOTIFICATION_BAR_OFFSET as u32,
            NOTIFICATION_SIZE as u32,
            Le32::from(NOTIFY_OFF_MULTIPLIER),
        );
        self.configuration
            .add_capability(&notify_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        //TODO(dgreid) - How will the configuration_cap work?
        let configuration_cap = VirtioPciCap::new(PciCapabilityType::PciConfig, 0, 0, 0);
        self.configuration
            .add_capability(&configuration_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        self.settings_bar = settings_bar;
        Ok(())
    }
}

impl PciDevice for VirtioPciDevice {
    fn assign_irq(&mut self, irq_evt: EventFd, irq_num: u32, irq_pin: PciInterruptPin) {
        self.configuration.set_irq(irq_num as u8, irq_pin);
        self.interrupt_evt = Some(irq_evt);
    }

    fn config_registers(&self) -> &PciConfiguration {
        &self.configuration
    }

    fn config_registers_mut(&mut self) -> &mut PciConfiguration {
        &mut self.configuration
    }

    fn ioeventfds(&self) -> Vec<(&EventFd, u64, u64)> {
        let bar0 = self
            .configuration
            .get_bar64_addr(self.settings_bar as usize);
        let notify_base = bar0 + NOTIFICATION_BAR_OFFSET;
        self.queue_evts()
            .iter()
            .enumerate()
            .map(|(i, event)| {
                (
                    event,
                    notify_base + i as u64 * u64::from(NOTIFY_OFF_MULTIPLIER),
                    i as u64,
                )
            })
            .collect()
    }

    fn allocate_bars(
        &mut self,
        allocator: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(GuestAddress, GuestUsize)>, PciDeviceError> {
        let mut ranges = Vec::new();

        // Allocate the virtio-pci capability BAR.
        // See http://docs.oasis-open.org/virtio/virtio/v1.0/cs04/virtio-v1.0-cs04.html#x1-740004
        let virtio_pci_bar_addr = allocator
            .allocate_mmio_addresses(None, CAPABILITY_BAR_SIZE)
            .ok_or(PciDeviceError::IoAllocationFailed(CAPABILITY_BAR_SIZE))?;
        let config = PciBarConfiguration::default()
            .set_register_index(0)
            .set_address(virtio_pci_bar_addr.raw_value())
            .set_size(CAPABILITY_BAR_SIZE);
        let virtio_pci_bar =
            self.configuration.add_pci_bar(&config).map_err(|e| {
                PciDeviceError::IoRegistrationFailed(virtio_pci_bar_addr.raw_value(), e)
            })? as u8;

        ranges.push((virtio_pci_bar_addr, CAPABILITY_BAR_SIZE));

        // Once the BARs are allocated, the capabilities can be added to the PCI configuration.
        self.add_pci_capabilities(virtio_pci_bar)?;

        // Allocate the device specific BARs.
        for config in self.device.get_device_bars() {
            let device_bar_addr = allocator
                .allocate_mmio_addresses(None, config.get_size())
                .ok_or_else(|| PciDeviceError::IoAllocationFailed(config.get_size()))?;
            config.set_address(device_bar_addr.raw_value());
            let _device_bar = self.configuration.add_pci_bar(&config).map_err(|e| {
                PciDeviceError::IoRegistrationFailed(device_bar_addr.raw_value(), e)
            })?;
            ranges.push((device_bar_addr, config.get_size()));
        }

        Ok(ranges)
    }

    fn read_bar(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            o if o < COMMON_CONFIG_BAR_OFFSET + COMMON_CONFIG_SIZE => self.common_config.read(
                o - COMMON_CONFIG_BAR_OFFSET,
                data,
                &mut self.queues,
                self.device.as_mut(),
            ),
            o if ISR_CONFIG_BAR_OFFSET <= o && o < ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE => {
                if let Some(v) = data.get_mut(0) {
                    // Reading this register resets it to 0.
                    *v = self.interrupt_status.swap(0, Ordering::SeqCst) as u8;
                }
            }
            o if DEVICE_CONFIG_BAR_OFFSET <= o
                && o < DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE =>
            {
                self.device.read_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if NOTIFICATION_BAR_OFFSET <= o
                && o < NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE =>
            {
                // Handled with ioeventfds.
            }
            _ => (),
        }
    }

    fn write_bar(&mut self, offset: u64, data: &[u8]) {
        match offset {
            o if o < COMMON_CONFIG_BAR_OFFSET + COMMON_CONFIG_SIZE => self.common_config.write(
                o - COMMON_CONFIG_BAR_OFFSET,
                data,
                &mut self.queues,
                self.device.as_mut(),
            ),
            o if ISR_CONFIG_BAR_OFFSET <= o && o < ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE => {
                if let Some(v) = data.get(0) {
                    self.interrupt_status
                        .fetch_and(!(*v as usize), Ordering::SeqCst);
                }
            }
            o if DEVICE_CONFIG_BAR_OFFSET <= o
                && o < DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE =>
            {
                self.device.write_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if NOTIFICATION_BAR_OFFSET <= o
                && o < NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE =>
            {
                // Handled with ioeventfds.
            }
            _ => (),
        };

        if !self.device_activated && self.is_driver_ready() && self.are_queues_valid() {
            if let Some(interrupt_evt) = self.interrupt_evt.take() {
                if self.memory.is_some() {
                    let mem = self.memory.as_ref().unwrap().clone();
                    self.device
                        .activate(
                            mem,
                            interrupt_evt,
                            self.interrupt_status.clone(),
                            self.queues.clone(),
                            self.queue_evts.split_off(0),
                        )
                        .expect("Failed to activate device");;
                    self.device_activated = true;
                }
            }
        }

        // Device has been reset by the driver
        if self.device_activated && self.is_driver_init() {
            if let Some((interrupt_evt, mut queue_evts)) = self.device.reset() {
                // Upon reset the device returns its interrupt EventFD and it's queue EventFDs
                self.interrupt_evt = Some(interrupt_evt);
                self.queue_evts.append(&mut queue_evts);

                self.device_activated = false;

                // Reset queue readiness (changes queue_enable), queue sizes
                // and selected_queue as per spec for reset
                self.queues.iter_mut().for_each(Queue::reset);
                self.common_config.queue_select = 0;
            } else {
                error!("Attempt to reset device when not implemented in underlying device");
                self.common_config.driver_status = crate::DEVICE_FAILED as u8;
            }
        }
    }
}

impl BusDevice for VirtioPciDevice {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        self.read_bar(offset, data)
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        self.write_bar(offset, data)
    }

    fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        if offset as usize + data.len() > 4 {
            return;
        }

        let regs = self.config_registers_mut();

        match data.len() {
            1 => regs.write_byte(reg_idx * 4 + offset as usize, data[0]),
            2 => regs.write_word(
                reg_idx * 4 + offset as usize,
                u16::from(data[0]) | (u16::from(data[1]) << 8),
            ),
            4 => regs.write_reg(reg_idx, LittleEndian::read_u32(data)),
            _ => (),
        }
    }

    fn read_config_register(&self, reg_idx: usize) -> u32 {
        self.config_registers().read_reg(reg_idx)
    }
}
